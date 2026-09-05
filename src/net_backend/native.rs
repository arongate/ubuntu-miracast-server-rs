//! Native, root-free networking backend (feature = `native-net`).
//!
//! Replaces `sudo ip` + `dnsmasq` with:
//! - **netlink** (via `neli`, sync) to flush + add the GO's static IP and bring
//!   the interface up — needs `CAP_NET_ADMIN`;
//! - an **in-process DHCP server** (plain `UdpSocket` on port 67, no external
//!   binary) that hands out the pool and records leases in memory — needs
//!   `CAP_NET_BIND_SERVICE` to bind the privileged port.
//!
//! Both capabilities are granted to the binary via `setcap` at install time
//! (see debian/postinst), so the app runs with NO runtime root. This backend
//! depends only on the Linux kernel — no NetworkManager, no `ip`/`dnsmasq`
//! binaries — making it the most portable option across distributions.
//!
//! DHCP behaviour mirrors the dnsmasq config it replaces:
//! `--dhcp-range=192.168.173.80,192.168.173.90,255.255.255.0,5m` with router
//! (opt 3) and DNS (opt 6) both pointing at the GO's IP.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{NetBackend, DHCP_POOL_END, DHCP_POOL_START, OUR_IP};

/// Shared MAC→IP lease table, populated by the DHCP server thread and read by
/// `wait_for_dhcp_lease`.
type LeaseTable = Arc<Mutex<HashMap<String, Ipv4Addr>>>;

pub struct NativeNetBackend {
    leases: LeaseTable,
    dhcp_started: Arc<AtomicBool>,
}

impl Default for NativeNetBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeNetBackend {
    pub fn new() -> Self {
        Self {
            leases: Arc::new(Mutex::new(HashMap::new())),
            dhcp_started: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl NetBackend for NativeNetBackend {
    fn setup_dhcp(&self, iface: &str) -> String {
        let our_ip = OUR_IP.to_string();

        // Configure the interface IP via netlink (CAP_NET_ADMIN).
        if let Err(e) = netlink::configure_interface(iface, Ipv4Addr::new(192, 168, 173, 1), 24) {
            log::error!("Failed to set up interface {iface} via netlink: {e}");
        }

        // Start the in-process DHCP server once.
        if !self.dhcp_started.swap(true, Ordering::SeqCst) {
            let leases = Arc::clone(&self.leases);
            let iface_owned = iface.to_string();
            std::thread::Builder::new()
                .name("dhcp-server".to_string())
                .spawn(move || dhcp::serve(&iface_owned, leases))
                .expect("spawn dhcp-server");
            log::info!("DHCP server started on {iface} ({our_ip}/24)");
        }
        our_ip
    }

    fn wait_for_dhcp_lease(
        &self,
        peer_mac: &str,
        _iface: &str,
        timeout: Duration,
        running: &AtomicBool,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        let mac_lower = peer_mac.to_lowercase();
        while Instant::now() < deadline && running.load(Ordering::SeqCst) {
            if let Some(ip) = self
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&mac_lower)
            {
                return Some(ip.to_string());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        None
    }
}

// ─── netlink interface configuration ─────────────────────────────────────────

mod netlink {
    use std::net::Ipv4Addr;

    use neli::consts::nl::{NlmF, Nlmsg};
    use neli::consts::rtnl::{Arphrd, Ifa, IfaF, RtAddrFamily, RtScope, Rtm};
    use neli::consts::socket::NlFamily;
    use neli::nl::{NlPayload, NlmsghdrBuilder};
    use neli::rtnl::{IfaddrmsgBuilder, IfinfomsgBuilder, RtattrBuilder};
    use neli::socket::synchronous::NlSocketHandle;
    use neli::types::RtBuffer;
    use neli::utils::Groups;

    /// Resolve an interface name to its kernel index via /sys (no ioctl needed).
    fn if_index(iface: &str) -> Result<i32, String> {
        let path = format!("/sys/class/net/{iface}/ifindex");
        std::fs::read_to_string(&path)
            .map_err(|e| format!("read {path}: {e}"))?
            .trim()
            .parse::<i32>()
            .map_err(|e| format!("parse ifindex: {e}"))
    }

    fn fmt<E: std::fmt::Display>(e: E) -> String {
        format!("netlink: {e}")
    }

    /// Add `ip/prefix` on the interface and bring it up (RTM_NEWADDR +
    /// RTM_NEWLINK with IFF_UP). Requires CAP_NET_ADMIN.
    pub fn configure_interface(iface: &str, ip: Ipv4Addr, prefix: u8) -> Result<(), String> {
        let index = if_index(iface)?;
        let sock = NlSocketHandle::connect(NlFamily::Route, None, Groups::empty()).map_err(fmt)?;

        // ── RTM_NEWADDR: add ip/prefix ──
        let local = RtattrBuilder::default()
            .rta_type(Ifa::Local)
            .rta_payload(ip.octets().to_vec())
            .build()
            .map_err(fmt)?;
        let address = RtattrBuilder::default()
            .rta_type(Ifa::Address)
            .rta_payload(ip.octets().to_vec())
            .build()
            .map_err(fmt)?;
        let mut addr_attrs = RtBuffer::new();
        addr_attrs.push(local);
        addr_attrs.push(address);

        let ifaddr = IfaddrmsgBuilder::default()
            .ifa_family(RtAddrFamily::Inet)
            .ifa_prefixlen(prefix)
            .ifa_flags(IfaF::empty())
            .ifa_scope(RtScope::Universe)
            .ifa_index(index as u32)
            .rtattrs(addr_attrs)
            .build()
            .map_err(fmt)?;

        let nl_addr = NlmsghdrBuilder::default()
            .nl_type(Rtm::Newaddr)
            .nl_flags(NlmF::REQUEST | NlmF::CREATE | NlmF::REPLACE | NlmF::ACK)
            .nl_payload(NlPayload::Payload(ifaddr))
            .build()
            .map_err(fmt)?;
        sock.send(&nl_addr).map_err(fmt)?;
        drain_ack(&sock);

        // ── RTM_NEWLINK: IFF_UP ──
        let ifinfo = IfinfomsgBuilder::default()
            .ifi_family(RtAddrFamily::Unspecified)
            .ifi_type(Arphrd::None)
            .ifi_index(index)
            .up()
            .rtattrs(RtBuffer::new())
            .build()
            .map_err(fmt)?;
        let nl_link = NlmsghdrBuilder::default()
            .nl_type(Rtm::Newlink)
            .nl_flags(NlmF::REQUEST | NlmF::ACK)
            .nl_payload(NlPayload::Payload(ifinfo))
            .build()
            .map_err(fmt)?;
        sock.send(&nl_link).map_err(fmt)?;
        drain_ack(&sock);

        log::debug!("netlink: configured {iface} ({ip}/{prefix}, up)");
        Ok(())
    }

    /// Read and discard the ACK/error message for the last request. A benign
    /// error (e.g. address already present) is not fatal — we mirror the CLI's
    /// tolerant `ip addr add` behaviour.
    fn drain_ack(sock: &NlSocketHandle) {
        // One recv drains the ACK/error for the last request; ignore contents.
        // A benign error (e.g. address already present) is not fatal.
        let _ = sock.recv::<Nlmsg, neli::types::Buffer>();
    }
}

// ─── in-process DHCP server ──────────────────────────────────────────────────

mod dhcp {
    use super::{LeaseTable, DHCP_POOL_END, DHCP_POOL_START};
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

    const SERVER_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 173, 1);
    const LEASE_SECS: u32 = 300; // dnsmasq used 5m

    // BOOTP/DHCP op + option codes.
    const OP_REPLY: u8 = 2;
    const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
    const OPT_MSG_TYPE: u8 = 53;
    const OPT_SERVER_ID: u8 = 54;
    const OPT_LEASE_TIME: u8 = 51;
    const OPT_SUBNET_MASK: u8 = 1;
    const OPT_ROUTER: u8 = 3;
    const OPT_DNS: u8 = 6;
    const OPT_REQUESTED_IP: u8 = 50;
    const OPT_END: u8 = 255;
    const DHCP_DISCOVER: u8 = 1;
    const DHCP_REQUEST: u8 = 3;
    const DHCP_OFFER: u8 = 2;
    const DHCP_ACK: u8 = 5;

    /// Bind UDP :67 (needs CAP_NET_BIND_SERVICE) and serve DISCOVER/REQUEST.
    pub fn serve(iface: &str, leases: LeaseTable) {
        let sock = match bind_to_iface(iface) {
            Ok(s) => s,
            Err(e) => {
                log::error!("DHCP: failed to bind :67 on {iface}: {e}");
                return;
            }
        };
        let mut next = DHCP_POOL_START[3];
        let mut buf = [0u8; 1024];
        loop {
            let (n, _from) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("DHCP recv error: {e}");
                    continue;
                }
            };
            if let Some(reply) = handle_packet(&buf[..n], &leases, &mut next) {
                // Reply broadcast — the client has no IP yet.
                let dest = SocketAddr::from((Ipv4Addr::BROADCAST, 68));
                if let Err(e) = sock.send_to(&reply, dest) {
                    log::debug!("DHCP send error: {e}");
                }
            }
        }
    }

    fn bind_to_iface(iface: &str) -> std::io::Result<UdpSocket> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 67))?;
        sock.set_broadcast(true)?;
        // Pin to the P2P interface via SO_BINDTODEVICE so we never answer DHCP
        // on the user's real LAN. Best-effort (needs the raw setsockopt).
        #[cfg(target_os = "linux")]
        bind_to_device(&sock, iface);
        Ok(sock)
    }

    #[cfg(target_os = "linux")]
    fn bind_to_device(sock: &UdpSocket, iface: &str) {
        use std::os::unix::io::AsRawFd;
        // SO_BINDTODEVICE = 25, SOL_SOCKET = 1.
        let fd = sock.as_raw_fd();
        let name = iface.as_bytes();
        // Safety: passing a valid fd + a byte buffer of the iface name length.
        let ret = unsafe {
            setsockopt(
                fd,
                1,  // SOL_SOCKET
                25, // SO_BINDTODEVICE
                name.as_ptr() as *const core::ffi::c_void,
                name.len() as u32,
            )
        };
        if ret != 0 {
            log::debug!("DHCP: SO_BINDTODEVICE({iface}) failed (continuing unbound)");
        }
    }

    #[cfg(target_os = "linux")]
    extern "C" {
        fn setsockopt(
            fd: i32,
            level: i32,
            optname: i32,
            optval: *const core::ffi::c_void,
            optlen: u32,
        ) -> i32;
    }

    /// Parse a DHCP request and build an OFFER/ACK; record the lease.
    fn handle_packet(pkt: &[u8], leases: &LeaseTable, next: &mut u8) -> Option<Vec<u8>> {
        // Minimum BOOTP header is 240 bytes incl. magic cookie.
        if pkt.len() < 240 || pkt[236..240] != MAGIC_COOKIE {
            return None;
        }
        let xid = &pkt[4..8];
        let chaddr = &pkt[28..34]; // 6-byte MAC
        let mac = format_mac(chaddr);

        // Walk options for the message type and any requested IP.
        let mut msg_type = 0u8;
        let mut requested_ip: Option<Ipv4Addr> = None;
        let mut i = 240;
        while i < pkt.len() {
            let code = pkt[i];
            if code == OPT_END {
                break;
            }
            if code == 0 {
                i += 1;
                continue;
            }
            if i + 1 >= pkt.len() {
                break;
            }
            let len = pkt[i + 1] as usize;
            let val_start = i + 2;
            let val_end = (val_start + len).min(pkt.len());
            let val = &pkt[val_start..val_end];
            match code {
                OPT_MSG_TYPE if !val.is_empty() => msg_type = val[0],
                OPT_REQUESTED_IP if val.len() == 4 => {
                    requested_ip = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3]))
                }
                _ => {}
            }
            i = val_end;
        }

        let reply_type = match msg_type {
            DHCP_DISCOVER => DHCP_OFFER,
            DHCP_REQUEST => DHCP_ACK,
            _ => return None,
        };

        // Assign (or reuse) an address from the pool.
        let assigned = {
            let mut table = leases.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = table.get(&mac) {
                *existing
            } else {
                let ip = requested_ip
                    .filter(in_pool)
                    .unwrap_or_else(|| alloc_from_pool(next));
                table.insert(mac.clone(), ip);
                ip
            }
        };
        if reply_type == DHCP_ACK {
            log::info!("DHCP: leased {assigned} to {mac}");
        }

        Some(build_reply(xid, chaddr, assigned, reply_type))
    }

    fn in_pool(ip: &Ipv4Addr) -> bool {
        let o = ip.octets();
        o[0] == DHCP_POOL_START[0]
            && o[1] == DHCP_POOL_START[1]
            && o[2] == DHCP_POOL_START[2]
            && o[3] >= DHCP_POOL_START[3]
            && o[3] <= DHCP_POOL_END[3]
    }

    fn alloc_from_pool(next: &mut u8) -> Ipv4Addr {
        let last = *next;
        *next = if last >= DHCP_POOL_END[3] {
            DHCP_POOL_START[3]
        } else {
            last + 1
        };
        Ipv4Addr::new(
            DHCP_POOL_START[0],
            DHCP_POOL_START[1],
            DHCP_POOL_START[2],
            last,
        )
    }

    fn build_reply(xid: &[u8], chaddr: &[u8], yiaddr: Ipv4Addr, msg_type: u8) -> Vec<u8> {
        let mut p = vec![0u8; 240];
        p[0] = OP_REPLY;
        p[1] = 1; // htype = Ethernet
        p[2] = 6; // hlen
        p[4..8].copy_from_slice(xid);
        p[16..20].copy_from_slice(&yiaddr.octets()); // yiaddr
        p[20..24].copy_from_slice(&SERVER_IP.octets()); // siaddr
        p[28..34].copy_from_slice(&chaddr[..6]); // chaddr
        p[236..240].copy_from_slice(&MAGIC_COOKIE);

        // Options (mirror dnsmasq: msg type, server id, lease, mask, router, dns).
        push_opt(&mut p, OPT_MSG_TYPE, &[msg_type]);
        push_opt(&mut p, OPT_SERVER_ID, &SERVER_IP.octets());
        push_opt(&mut p, OPT_LEASE_TIME, &LEASE_SECS.to_be_bytes());
        push_opt(&mut p, OPT_SUBNET_MASK, &[255, 255, 255, 0]);
        push_opt(&mut p, OPT_ROUTER, &SERVER_IP.octets());
        push_opt(&mut p, OPT_DNS, &SERVER_IP.octets());
        p.push(OPT_END);
        p
    }

    fn push_opt(p: &mut Vec<u8>, code: u8, val: &[u8]) {
        p.push(code);
        p.push(val.len() as u8);
        p.extend_from_slice(val);
    }

    fn format_mac(mac: &[u8]) -> String {
        mac.iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn discover_packet(mac: [u8; 6]) -> Vec<u8> {
            let mut p = vec![0u8; 240];
            p[0] = 1; // op = request
            p[1] = 1;
            p[2] = 6;
            p[4..8].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
            p[28..34].copy_from_slice(&mac);
            p[236..240].copy_from_slice(&MAGIC_COOKIE);
            push_opt(&mut p, OPT_MSG_TYPE, &[DHCP_DISCOVER]);
            p.push(OPT_END);
            p
        }

        #[test]
        fn discover_yields_offer_in_pool_and_records_lease() {
            let leases: LeaseTable =
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            let mut next = DHCP_POOL_START[3];
            let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
            let reply = handle_packet(&discover_packet(mac), &leases, &mut next).expect("offer");
            // op=reply, yiaddr in pool.
            assert_eq!(reply[0], OP_REPLY);
            let yiaddr = Ipv4Addr::new(reply[16], reply[17], reply[18], reply[19]);
            assert!(in_pool(&yiaddr), "offered {yiaddr} not in pool");
            // Lease recorded under the lowercase MAC.
            assert!(leases.lock().unwrap().contains_key("00:11:22:33:44:55"));
        }

        #[test]
        fn reply_carries_router_and_dns_options() {
            let reply = build_reply(&[1, 2, 3, 4], &[0, 1, 2, 3, 4, 5], SERVER_IP, DHCP_ACK);
            // Router (3) and DNS (6) both = server IP, like dnsmasq opts 3/6.
            assert!(find_opt(&reply, OPT_ROUTER) == Some(SERVER_IP.octets().to_vec()));
            assert!(find_opt(&reply, OPT_DNS) == Some(SERVER_IP.octets().to_vec()));
            assert!(find_opt(&reply, OPT_SUBNET_MASK) == Some(vec![255, 255, 255, 0]));
        }

        fn find_opt(pkt: &[u8], code: u8) -> Option<Vec<u8>> {
            let mut i = 240;
            while i < pkt.len() && pkt[i] != OPT_END {
                if pkt[i] == 0 {
                    i += 1;
                    continue;
                }
                let len = pkt[i + 1] as usize;
                if pkt[i] == code {
                    return Some(pkt[i + 2..i + 2 + len].to_vec());
                }
                i += 2 + len;
            }
            None
        }

        #[test]
        fn pool_wraps_at_end() {
            let mut next = DHCP_POOL_END[3];
            let _ = alloc_from_pool(&mut next);
            assert_eq!(next, DHCP_POOL_START[3]);
        }
    }
}
