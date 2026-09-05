//! Subprocess networking backend: `sudo ip` + `dnsmasq` (DEFAULT).
//!
//! Faithful relocation of the previous inline `setup_dhcp` / `wait_for_dhcp_lease`
//! in `connection.rs` — identical argv, identical dnsmasq flags. Needs root.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{NetBackend, OUR_IP};

pub struct SubprocessNetBackend;

impl NetBackend for SubprocessNetBackend {
    fn setup_dhcp(&self, iface: &str) -> String {
        let our_ip = OUR_IP.to_string();

        // Kill any stale dnsmasq on this interface from previous runs.
        let _ = Command::new("sudo")
            .args(["pkill", "-f", &format!("dnsmasq.*{iface}")])
            .output();
        std::thread::sleep(Duration::from_millis(300));

        let _ = Command::new("sudo")
            .args(["ip", "addr", "flush", "dev", iface])
            .output();
        let _ = Command::new("sudo")
            .args(["ip", "addr", "add", &format!("{our_ip}/24"), "dev", iface])
            .output();
        let _ = Command::new("sudo")
            .args(["ip", "link", "set", iface, "up"])
            .output();

        let spawn = Command::new("sudo")
            .args([
                "dnsmasq",
                &format!("--interface={iface}"),
                "--bind-interfaces",
                "--dhcp-range=192.168.173.80,192.168.173.90,255.255.255.0,5m",
                &format!("--dhcp-option=3,{our_ip}"),
                &format!("--dhcp-option=6,{our_ip}"),
                "--no-daemon",
                "--log-facility=-",
                "--except-interface=lo",
                "--no-resolv",
                "--no-hosts",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match spawn {
            Ok(_) => log::info!("DHCP server started on {iface} ({our_ip}/24)"),
            Err(e) => log::error!("Failed to set up DHCP: {e}"),
        }
        our_ip
    }

    fn wait_for_dhcp_lease(
        &self,
        peer_mac: &str,
        iface: &str,
        timeout: Duration,
        running: &AtomicBool,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        let mac_lower = peer_mac.to_lowercase();

        while Instant::now() < deadline && running.load(Ordering::SeqCst) {
            if let Ok(content) = std::fs::read_to_string("/var/lib/misc/dnsmasq.leases") {
                for line in content.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 3 && parts[1].to_lowercase() == mac_lower {
                        return Some(parts[2].to_string());
                    }
                }
            }
            if let Ok(out) = Command::new("ip")
                .args(["neigh", "show", "dev", iface])
                .output()
            {
                for line in String::from_utf8_lossy(&out.stdout).trim().lines() {
                    if line.to_lowercase().contains(&mac_lower) {
                        if let Some(ip) = line.split_whitespace().next() {
                            return Some(ip.to_string());
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        None
    }
}
