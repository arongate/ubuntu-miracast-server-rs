//! Wi-Fi Direct Connection Handler for Ubuntu Miracast Server.
//!
//! Autonomous GO approach: monitors the GROUP interface for peer connect
//! events after arming a WPS PIN. The P2P control plane (event monitoring,
//! WPS arming) is delegated to [`P2pBackend`]; this module owns PIN generation,
//! DHCP/IP setup, peer-IP resolution, and the app-facing event emission.
//!
//! Flow:
//!   1. Receive group interface name from advertiser
//!   2. Generate and display a WPS PIN, arm it via the backend
//!   3. Backend delivers `PeerConnected` when a source joins
//!   4. Set up DHCP on the group interface, resolve the peer IP
//!   5. Emit connection-received with peer details

use crate::events::{Event, EventSender};
use crate::models::IncomingConnection;
use crate::net_backend::NetBackend;
use crate::p2p_backend::{P2pBackend, P2pEvent};
use crate::sync_ext::LockExt;
use chrono::Local;
use rand::Rng;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The GO's static IP on the group interface (matches the Python default).
const OUR_IP: &str = "192.168.173.1";
/// Fallback peer IP if no DHCP lease is found (first of the range).
const FALLBACK_PEER_IP: &str = "192.168.173.80";

fn generate_pin() -> String {
    // 8-digit WPS PIN — non-crypto, exactly like Python random.randint.
    let n: u32 = rand::thread_rng().gen_range(10_000_000..=99_999_999);
    n.to_string()
}

/// Shared state accessible from the monitor thread and the owner.
struct Shared {
    running: AtomicBool,
    active_connection: Mutex<Option<IncomingConnection>>,
    current_pin: Mutex<Option<String>>,
    group_interface: Mutex<Option<String>>,
    our_ip: Mutex<String>,
    events: EventSender,
    backend: Arc<dyn P2pBackend>,
    net: Arc<dyn NetBackend>,
}

impl Shared {
    /// Arm the current PIN on the group interface via the backend.
    fn arm_current_pin(&self) {
        let group = self.group_interface.lock_safe().clone();
        let pin = self.current_pin.lock_safe().clone();
        if let (Some(group), Some(pin)) = (group, pin) {
            if let Err(e) = self.backend.arm_wps_pin(&group, &pin) {
                let _ = self.events.send(Event::ConnectionError(e.to_string()));
            }
        }
    }
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
        backend: Arc<dyn P2pBackend>,
        net: Arc<dyn NetBackend>,
        events: EventSender,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                running: AtomicBool::new(false),
                active_connection: Mutex::new(None),
                current_pin: Mutex::new(None),
                group_interface: Mutex::new(None),
                our_ip: Mutex::new(OUR_IP.to_string()),
                events,
                backend,
                net,
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
        self.shared.active_connection.lock_safe().clone()
    }

    pub fn set_p2p_interface(&mut self, iface: impl Into<String>) {
        self.p2p_interface = iface.into();
    }

    /// Start listening on the P2P group interface: sets up IP/DHCP, arms WPS
    /// PIN, and consumes backend P2P events.
    pub fn start_listening(&mut self, group_interface: impl Into<String>) {
        if self.shared.running.load(Ordering::SeqCst) {
            log::debug!("Already listening — ignoring");
            return;
        }
        let group_interface = group_interface.into();
        *self.shared.group_interface.lock_safe() = Some(group_interface.clone());
        self.shared.running.store(true, Ordering::SeqCst);

        // NOTE: DHCP setup + WPS arm block for seconds. The caller is the GTK
        // main loop's event drain, so we do ALL of it on the worker thread and
        // return immediately (avoids the "not responding" freeze).
        let shared = Arc::clone(&self.shared);
        let group = group_interface.clone();
        self.thread = Some(
            std::thread::Builder::new()
                .name("go-event-monitor".to_string())
                .spawn(move || {
                    // IP + DHCP first (before any client tries to connect).
                    let our_ip = shared.net.setup_dhcp(&group);
                    *shared.our_ip.lock_safe() = our_ip;

                    // Generate + arm the initial WPS PIN.
                    let pin = generate_pin();
                    *shared.current_pin.lock_safe() = Some(pin.clone());
                    let _ = shared.events.send(Event::PinDisplay {
                        pin: pin.clone(),
                        peer_info: "Waiting for source...".to_string(),
                    });
                    shared.arm_current_pin();
                    log::info!("Listening on {group} with PIN {pin}");

                    // The backend runs its own event source (a wpa_cli ATTACH
                    // loop, or D-Bus signals) and feeds P2pEvents down a
                    // channel; a second thread translates those into app events.
                    let (tx, rx) = std::sync::mpsc::channel::<P2pEvent>();
                    let drain = {
                        let shared = Arc::clone(&shared);
                        let group = group.clone();
                        std::thread::Builder::new()
                            .name("go-event-drain".to_string())
                            .spawn(move || event_drain_loop(shared, group, rx))
                            .expect("spawn go-event-drain")
                    };

                    shared
                        .backend
                        .run_event_monitor(&group, tx, &shared.running);

                    // Backend monitor returned → stop the drain and join it.
                    shared.running.store(false, Ordering::SeqCst);
                    let _ = drain.join();
                })
                .expect("spawn go-event-monitor"),
        );
    }

    /// Stop listening for connections.
    pub fn stop_listening(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
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
        *self.shared.active_connection.lock_safe() = None;
    }

    /// Generate a new PIN and re-arm WPS for the next connection.
    ///
    /// Called from the GTK event drain, so the (possibly slow) arm runs on a
    /// detached thread rather than blocking the main loop.
    pub fn rearm_wps_pin(&self) {
        let pin = generate_pin();
        *self.shared.current_pin.lock_safe() = Some(pin.clone());
        let _ = self.shared.events.send(Event::PinDisplay {
            pin: pin.clone(),
            peer_info: "Waiting for source...".to_string(),
        });
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || {
            shared.arm_current_pin();
            log::info!("Re-armed WPS with new PIN {pin}");
        });
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        if self.shared.running.load(Ordering::SeqCst) {
            self.stop_listening();
        }
    }
}

/// Translate backend [`P2pEvent`]s into app [`Event`]s (DHCP, peer IP, WPS
/// re-arm). Runs on its own thread so a slow DHCP-lease wait never blocks the
/// backend's event source.
fn event_drain_loop(shared: Arc<Shared>, group: String, rx: Receiver<P2pEvent>) {
    while shared.running.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(P2pEvent::PeerConnected { mac }) => handle_sta_connected(&shared, &mac, &group),
            Ok(P2pEvent::PeerDisconnected) => handle_sta_disconnected(&shared),
            Ok(P2pEvent::WpsPinNeeded) => {
                let pin = shared.current_pin.lock_safe().clone().unwrap_or_default();
                log::warn!("WPS re-arm requested: PIN {pin}");
                shared.arm_current_pin();
            }
            Ok(P2pEvent::GroupRemoved) => {
                log::info!("P2P group removed");
                break;
            }
            Ok(P2pEvent::Error(e)) => {
                let _ = shared.events.send(Event::ConnectionError(e));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn handle_sta_connected(shared: &Arc<Shared>, peer_mac: &str, group: &str) {
    log::info!("Source connected: {peer_mac}");
    let our_ip = shared.our_ip.lock_safe().clone();

    let peer_ip = shared
        .net
        .wait_for_dhcp_lease(peer_mac, group, Duration::from_secs(15), &shared.running)
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
            *shared.active_connection.lock_safe() = Some(conn.clone());
            let _ = shared.events.send(Event::ConnectionReceived(conn));
        }
        Err(e) => {
            log::error!("Rejected malformed connection: {e}");
            let _ = shared.events.send(Event::ConnectionError(e.to_string()));
        }
    }
}

/// Wait for a DHCP lease for `peer_mac`. Polls dnsmasq leases and `ip neigh`.
fn handle_sta_disconnected(shared: &Arc<Shared>) {
    let was_connected = {
        let mut guard = shared.active_connection.lock_safe();
        let was = guard.is_some();
        *guard = None;
        was
    };
    if was_connected {
        log::info!("Source disconnected");
        let _ = shared.events.send(Event::ConnectionLost);
        // Re-arm WPS for the next connection.
        let pin = generate_pin();
        *shared.current_pin.lock_safe() = Some(pin.clone());
        shared.arm_current_pin();
        let _ = shared.events.send(Event::PinDisplay {
            pin: pin.clone(),
            peer_info: "Waiting for source...".to_string(),
        });
        log::info!("Re-armed WPS with new PIN {pin}");
    }
}

/// Set up IP addressing on the group interface: static IP + dnsmasq DHCP.
/// Returns our IP. (Still subprocess — IP/DHCP is Phase 3, not Phase 2.)
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
}
