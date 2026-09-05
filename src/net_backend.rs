//! Networking backend abstraction: interface IP configuration + DHCP.
//!
//! The connection handler sets a static IP on the P2P group interface and runs
//! a DHCP server so the source (phone) gets an address, then resolves that
//! address. Two backends implement this:
//!
//! - [`subprocess`](self::subprocess) — the original `sudo ip` + `dnsmasq`
//!   path (DEFAULT, hardware-validated). Needs root.
//! - `native` (feature = `native-net`) — netlink for the interface IP + an
//!   in-process DHCP server, needing only `CAP_NET_ADMIN` +
//!   `CAP_NET_BIND_SERVICE` (granted via `setcap`, no runtime root). Depends on
//!   nothing beyond the Linux kernel — no NetworkManager, no `ip`/`dnsmasq`
//!   binaries — which is the most portable option across Linux distributions.
//!
//! Selection is at runtime (see [`select_net_backend`]): the native backend is
//! used only when compiled in AND `MIRACAST_NET=native` is set.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// The GO's static IP on the group interface (matches the Python default).
pub const OUR_IP: &str = "192.168.173.1";
/// DHCP pool start/end (matches the Python dnsmasq --dhcp-range).
pub const DHCP_POOL_START: [u8; 4] = [192, 168, 173, 80];
pub const DHCP_POOL_END: [u8; 4] = [192, 168, 173, 90];

/// IP configuration + DHCP for the P2P group interface.
pub trait NetBackend: Send + Sync {
    /// Assign the GO's static IP to `iface`, bring it up, and start a DHCP
    /// server handing out the pool. Returns our IP (always [`OUR_IP`]).
    /// Best-effort like the Python: logs on failure, never panics.
    fn setup_dhcp(&self, iface: &str) -> String;

    /// Wait up to `timeout` for `peer_mac` to acquire a lease; return its IP.
    /// `running` allows cooperative cancellation.
    fn wait_for_dhcp_lease(
        &self,
        peer_mac: &str,
        iface: &str,
        timeout: Duration,
        running: &AtomicBool,
    ) -> Option<String>;
}

pub mod subprocess;

#[cfg(feature = "native-net")]
pub mod native;

/// Choose the networking backend at runtime. Defaults to the subprocess
/// (`ip`/`dnsmasq`) backend; selects the native (setcap) backend only when
/// compiled in AND `MIRACAST_NET=native` is set.
pub fn select_net_backend() -> std::sync::Arc<dyn NetBackend> {
    #[cfg(feature = "native-net")]
    {
        let want_native = std::env::var("MIRACAST_NET")
            .map(|v| v.eq_ignore_ascii_case("native"))
            .unwrap_or(false);
        if want_native {
            log::info!("Net backend: native netlink + in-process DHCP");
            return std::sync::Arc::new(native::NativeNetBackend::new());
        }
    }
    log::info!("Net backend: subprocess (ip + dnsmasq)");
    std::sync::Arc::new(subprocess::SubprocessNetBackend)
}
