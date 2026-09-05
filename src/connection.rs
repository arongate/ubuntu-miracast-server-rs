//! Wi-Fi Direct Connection Handler for Ubuntu Miracast Server.
//!
//! Autonomous GO approach: monitors the GROUP interface for AP-STA-CONNECTED
//! events after arming a WPS PIN. Faithful port of
//! `src/miracast_server/connection.py`.
//!
//! Flow:
//!   1. Receive group interface name from advertiser
//!   2. Generate and display a WPS PIN
//!   3. Arm the GO's WPS registrar: `wps_pin any <PIN>`
//!   4. Wait for AP-STA-CONNECTED (source connected)
//!   5. Set up DHCP on the group interface
//!   6. Emit connection-received with peer details

use crate::events::{Event, EventSender};
use crate::models::IncomingConnection;
use crate::utils::run_wpa_cli;
use chrono::Local;
use rand::Rng;
use regex::Regex;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The GO's static IP on the group interface (matches the Python default).
const OUR_IP: &str = "192.168.173.1";
/// Fallback peer IP if no DHCP lease is found (first of the range).
const FALLBACK_PEER_IP: &str = "192.168.173.80";
/// Re-arm the WPS PIN this often (registrar timeout is ~120s).
const WPS_REARM_INTERVAL: Duration = Duration::from_secs(90);

fn generate_pin() -> String {
    // 8-digit WPS PIN — non-crypto, exactly like Python random.randint.
    let n: u32 = rand::thread_rng().gen_range(10_000_000..=99_999_999);
    n.to_string()
}

fn ap_sta_connected_re() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"AP-STA-CONNECTED\s+([0-9a-fA-F:]{17})").unwrap())
}
fn ap_sta_disconnected_re() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"AP-STA-DISCONNECTED\s+([0-9a-fA-F:]{17})").unwrap())
}

/// Shared state accessible from the monitor thread and the owner.
struct Shared {
    running: AtomicBool,
    active_connection: Mutex<Option<IncomingConnection>>,
    current_pin: Mutex<Option<String>>,
    group_interface: Mutex<Option<String>>,
    ctrl_path: Mutex<Option<String>>,
    our_ip: Mutex<String>,
    events: EventSender,
}

/// Handles Wi-Fi Direct P2P connections via WPS on the Group Owner interface.
pub struct ConnectionHandler {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    #[allow(dead_code)]
    go_intent: i32,
    #[allow(dead_code)]
    auto_accept: bool,
    #[allow(dead_code)]
    connection_timeout: i32,
    p2p_interface: String,
}

impl ConnectionHandler {
    pub fn new(
        p2p_interface: impl Into<String>,
        go_intent: i32,
        auto_accept: bool,
        connection_timeout: i32,
        events: EventSender,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                running: AtomicBool::new(false),
                active_connection: Mutex::new(None),
                current_pin: Mutex::new(None),
                group_interface: Mutex::new(None),
                ctrl_path: Mutex::new(None),
                our_ip: Mutex::new(OUR_IP.to_string()),
                events,
            }),
            thread: None,
            go_intent,
            auto_accept,
            connection_timeout,
            p2p_interface: p2p_interface.into(),
        }
    }

    pub fn is_listening(&self) -> bool {
        self.shared.running.load(Ordering::SeqCst)
    }

    pub fn active_connection(&self) -> Option<IncomingConnection> {
        self.shared.active_connection.lock().unwrap().clone()
    }

    pub fn set_ctrl_path(&self, ctrl_path: Option<String>) {
        *self.shared.ctrl_path.lock().unwrap() = ctrl_path;
    }

    pub fn set_p2p_interface(&mut self, iface: impl Into<String>) {
        self.p2p_interface = iface.into();
    }

    /// Start listening on the P2P group interface: sets up IP/DHCP immediately,
    /// arms WPS PIN, and monitors for events.
    pub fn start_listening(&mut self, group_interface: impl Into<String>) {
        if self.shared.running.load(Ordering::SeqCst) {
            log::debug!("Already listening — ignoring");
            return;
        }
        let group_interface = group_interface.into();
        *self.shared.group_interface.lock().unwrap() = Some(group_interface.clone());
        self.shared.running.store(true, Ordering::SeqCst);

        // Set up IP + DHCP FIRST (before any client tries to connect).
        let our_ip = setup_dhcp(&group_interface);
        *self.shared.our_ip.lock().unwrap() = our_ip;

        // Generate + arm WPS PIN.
        let pin = generate_pin();
        *self.shared.current_pin.lock().unwrap() = Some(pin.clone());
        arm_wps_pin(&self.shared);

        let _ = self.shared.events.send(Event::PinDisplay {
            pin: pin.clone(),
            peer_info: "Waiting for source...".to_string(),
        });

        // Start the event monitor thread on the group interface.
        let shared = Arc::clone(&self.shared);
        self.thread = Some(
            std::thread::Builder::new()
                .name("go-event-monitor".to_string())
                .spawn(move || event_monitor_loop(shared))
                .expect("spawn go-event-monitor"),
        );
        log::info!("Listening on {group_interface} with PIN {pin}");
    }

    /// Stop listening for connections.
    pub fn stop_listening(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            // 5s join budget (matches Python join(timeout=5)); detach if slow.
            let start = Instant::now();
            while !handle.is_finished() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(50));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                log::warn!("Event monitor thread did not stop within 5 seconds");
            }
        }
        log::info!("Connection handler stopped");
    }

    pub fn disconnect_peer(&self) {
        *self.shared.active_connection.lock().unwrap() = None;
    }

    /// Generate a new PIN and re-arm WPS for the next connection.
    pub fn rearm_wps_pin(&self) {
        let pin = generate_pin();
        *self.shared.current_pin.lock().unwrap() = Some(pin.clone());
        arm_wps_pin(&self.shared);
        let _ = self.shared.events.send(Event::PinDisplay {
            pin: pin.clone(),
            peer_info: "Waiting for source...".to_string(),
        });
        log::info!("Re-armed WPS with new PIN {pin}");
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        if self.shared.running.load(Ordering::SeqCst) {
            self.stop_listening();
        }
    }
}

/// Arm the WPS registrar on the group interface with the current PIN.
/// Retries up to 10 times with 1s delays (control socket may not be ready).
fn arm_wps_pin(shared: &Arc<Shared>) {
    let group = shared.group_interface.lock().unwrap().clone();
    let pin = shared.current_pin.lock().unwrap().clone();
    let ctrl = shared.ctrl_path.lock().unwrap().clone();
    let (group, pin) = match (group, pin) {
        (Some(g), Some(p)) => (g, p),
        _ => return,
    };

    for attempt in 0..10 {
        match run_wpa_cli(&group, &["wps_pin", "any", &pin], false, ctrl.as_deref()) {
            Ok(result) if !result.contains("FAIL") => {
                log::info!("WPS PIN armed: {pin} on {group} (attempt {})", attempt + 1);
                return;
            }
            Ok(_) => log::debug!("wps_pin attempt {} failed, retrying...", attempt + 1),
            Err(e) => log::debug!("wps_pin attempt {} error: {e}", attempt + 1),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    log::error!("Failed to arm WPS PIN after 10 attempts");
    let _ = shared.events.send(Event::ConnectionError(
        "Failed to arm WPS PIN — group interface not ready".to_string(),
    ));
}

/// Monitor the GROUP interface for AP-STA-CONNECTED events.
fn event_monitor_loop(shared: Arc<Shared>) {
    let group = shared
        .group_interface
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    let ctrl = shared.ctrl_path.lock().unwrap().clone();
    log::info!("Event monitor starting on group interface {group}");

    let mut cmd = Command::new("sudo");
    cmd.arg("wpa_cli");
    if let Some(p) = &ctrl {
        cmd.arg("-p").arg(p);
    }
    cmd.arg("-i").arg(&group);

    let mut proc = match cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(p) => p,
        Err(e) => {
            log::error!("Failed to start wpa_cli: {e}");
            let _ = shared.events.send(Event::ConnectionError(format!(
                "Failed to start event monitor: {e}"
            )));
            return;
        }
    };

    // Wait for the process to be ready.
    std::thread::sleep(Duration::from_millis(500));
    if let Ok(Some(_)) = proc.try_wait() {
        let stderr = proc
            .stderr
            .take()
            .map(|mut e| {
                let mut s = String::new();
                let _ = e.read_to_string(&mut s);
                s
            })
            .unwrap_or_default();
        log::error!("wpa_cli exited immediately: {}", stderr.trim());
        let _ = shared.events.send(Event::ConnectionError(format!(
            "wpa_cli failed: {}",
            stderr.trim()
        )));
        return;
    }

    let stdout = proc.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);

    // Drain banner, then ATTACH.
    drain_banner(&mut reader);
    if let Some(stdin) = proc.stdin.as_mut() {
        let _ = stdin.write_all(b"ATTACH\n");
        let _ = stdin.flush();
    }
    std::thread::sleep(Duration::from_millis(500));
    drain_output(&mut reader, Duration::from_secs(1));

    log::info!("Event monitor attached to {group} — waiting for connections");

    let mut last_rearm = Instant::now();

    while shared.running.load(Ordering::SeqCst) {
        if last_rearm.elapsed() >= WPS_REARM_INTERVAL {
            arm_wps_pin(&shared);
            last_rearm = Instant::now();
        }

        // Poll for a line with a ~1s cadence. BufRead has no timeout, so we
        // read line-by-line; wpa_cli in ATTACH mode streams events promptly,
        // and the running flag is re-checked each iteration.
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => {
                if let Ok(Some(_)) = proc.try_wait() {
                    log::error!("wpa_cli process died");
                    break;
                }
                continue;
            }
        }

        let mut line = line.trim().to_string();
        if line.is_empty() || line == ">" || line == "> " {
            continue;
        }
        if let Some(rest) = line.strip_prefix("> ") {
            line = rest.to_string();
        } else if let Some(rest) = line.strip_prefix('>') {
            line = rest.to_string();
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        if line.contains("AP-STA")
            || line.contains("P2P")
            || line.contains("WPS")
            || line.contains("CTRL")
        {
            log::info!("GO event: {line}");
        }

        if let Some(cap) = ap_sta_connected_re().captures(&line) {
            let peer_mac = cap[1].to_string();
            handle_sta_connected(&shared, &peer_mac, &group);
            continue;
        }
        if line.contains("WPS-PIN-NEEDED") {
            let pin = shared
                .current_pin
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default();
            log::warn!("WPS-PIN-NEEDED: re-arming PIN {pin}");
            arm_wps_pin(&shared);
            continue;
        }
        if ap_sta_disconnected_re().is_match(&line) {
            handle_sta_disconnected(&shared);
            continue;
        }
        if line.contains("P2P-GROUP-REMOVED") {
            log::info!("P2P group removed");
            break;
        }
    }

    // Cleanup: QUIT, terminate, kill.
    if let Some(stdin) = proc.stdin.as_mut() {
        let _ = stdin.write_all(b"QUIT\n");
        let _ = stdin.flush();
    }
    let _ = proc.kill();
    let _ = proc.wait();
    log::info!("Event monitor thread exiting");
}

fn handle_sta_connected(shared: &Arc<Shared>, peer_mac: &str, group: &str) {
    log::info!("Source connected: {peer_mac}");
    let our_ip = shared.our_ip.lock().unwrap().clone();

    // Wait for DHCP lease (up to 15s), else fall back to the first range IP.
    let peer_ip = wait_for_dhcp_lease(shared, peer_mac, group, Duration::from_secs(15))
        .unwrap_or_else(|| {
            log::warn!("Could not find DHCP lease for {peer_mac}, using {FALLBACK_PEER_IP}");
            FALLBACK_PEER_IP.to_string()
        });
    log::info!("Source {peer_mac} got IP {peer_ip}");

    match IncomingConnection::try_new(
        peer_mac,
        &peer_ip,
        "Miracast Source",
        group,
        &our_ip,
        Local::now(),
        true,
    ) {
        Ok(conn) => {
            *shared.active_connection.lock().unwrap() = Some(conn.clone());
            let _ = shared.events.send(Event::ConnectionReceived(conn));
        }
        Err(e) => {
            log::error!("Rejected malformed connection: {e}");
            let _ = shared.events.send(Event::ConnectionError(e.to_string()));
        }
    }
}

/// Wait for a DHCP lease for `peer_mac`. Polls dnsmasq leases and `ip neigh`.
fn wait_for_dhcp_lease(
    shared: &Arc<Shared>,
    peer_mac: &str,
    group: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mac_lower = peer_mac.to_lowercase();

    while Instant::now() < deadline && shared.running.load(Ordering::SeqCst) {
        // dnsmasq leases file.
        if let Ok(content) = std::fs::read_to_string("/var/lib/misc/dnsmasq.leases") {
            for line in content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 && parts[1].to_lowercase() == mac_lower {
                    return Some(parts[2].to_string());
                }
            }
        }
        // ARP / neighbour table.
        if let Ok(out) = Command::new("ip")
            .args(["neigh", "show", "dev", group])
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

fn handle_sta_disconnected(shared: &Arc<Shared>) {
    let was_connected = {
        let mut guard = shared.active_connection.lock().unwrap();
        let was = guard.is_some();
        *guard = None;
        was
    };
    if was_connected {
        log::info!("Source disconnected");
        let _ = shared.events.send(Event::ConnectionLost);
        // Re-arm WPS for the next connection.
        let pin = generate_pin();
        *shared.current_pin.lock().unwrap() = Some(pin.clone());
        arm_wps_pin(shared);
        let _ = shared.events.send(Event::PinDisplay {
            pin: pin.clone(),
            peer_info: "Waiting for source...".to_string(),
        });
        log::info!("Re-armed WPS with new PIN {pin}");
    }
}

/// Set up IP addressing on the group interface: static IP + dnsmasq DHCP.
/// Returns our IP. Uses the exact argv from the Python source.
fn setup_dhcp(iface: &str) -> String {
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

    // Start dnsmasq for DHCP with router option (identical argv).
    let spawn = Command::new("sudo")
        .args([
            "dnsmasq",
            &format!("--interface={iface}"),
            "--bind-interfaces",
            "--dhcp-range=192.168.173.80,192.168.173.90,255.255.255.0,5m",
            &format!("--dhcp-option=3,{our_ip}"), // Router/gateway
            &format!("--dhcp-option=6,{our_ip}"), // DNS
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

/// Read and discard the wpa_cli banner lines.
fn drain_banner<R: BufRead>(reader: &mut R) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let stripped = line.trim();
                if stripped.contains("Interactive mode") || stripped == ">" {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Read and discard output for a given timeout.
fn drain_output<R: BufRead>(reader: &mut R, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

#[allow(dead_code)]
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_eight_digits() {
        for _ in 0..100 {
            let pin = generate_pin();
            assert_eq!(pin.len(), 8);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn ap_sta_connected_pattern_extracts_mac() {
        let caps = ap_sta_connected_re()
            .captures("<3>AP-STA-CONNECTED 00:11:22:33:44:55 p2p_dev_addr=...")
            .unwrap();
        assert_eq!(&caps[1], "00:11:22:33:44:55");
    }

    #[test]
    fn ap_sta_disconnected_pattern_matches() {
        assert!(ap_sta_disconnected_re().is_match("<3>AP-STA-DISCONNECTED aa:bb:cc:dd:ee:ff"));
        assert!(!ap_sta_disconnected_re().is_match("AP-STA-CONNECTED aa:bb:cc:dd:ee:ff"));
    }
}
