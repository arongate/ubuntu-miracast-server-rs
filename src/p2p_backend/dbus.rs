//! `fi.w1.wpa_supplicant1` (system-bus D-Bus) implementation of [`P2pBackend`].
//!
//! This is the OPT-IN backend (feature `dbus-backend`, selected at runtime with
//! `MIRACAST_BACKEND=dbus`). It replaces the `wpa_cli` subprocess + interactive
//! ATTACH stream in [`cli`](super::cli) with `zbus::blocking` method calls and
//! signal subscriptions against wpa_supplicant's documented D-Bus surface:
//!
//! - root `WFDIEs` (`ay`, writable) — the `wifi_display 1` + `wfd_subelem_set`
//!   advertisement (see `docs/native-dbus-research.md`);
//! - `Interface.P2PDevice.P2PDeviceConfig` (`a{sv}`) — device name + GO intent;
//! - `P2PDevice.Find` / `GroupAdd` — autonomous Group Owner;
//! - `Interface.WPS.Start` — arm the WPS registrar with a PIN;
//! - `GroupStarted` / `PeerJoined` / `PeerDisconnected` / `GroupFinished`
//!   signals — event-driven, no `ip link` polling and no stdout scraping.
//!
//! Byte-for-byte fidelity with the CLI backend is preserved: the WFDIEs payload
//! is the hex-decode of [`cli::encode_wfd_device_info`] plus the same associated
//! -BSSID / coupled-sink subelements, and log messages mirror `cli.rs`.
//!
//! Sync only: `zbus::blocking::{Connection, Proxy}` — no tokio, no async, no
//! `#[proxy]` macro (dynamic `Proxy::new` + `call_method` / `get_property` /
//! `set_property` / `receive_signal`).
//!
//! Access is gated by the `netdev` D-Bus group policy (NOT polkit), so a user in
//! `netdev` drives all of this with no `sudo` and no prompt.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, Value};

use super::cli;
use super::{BackendError, BackendResult, P2pBackend, P2pEvent};

// ---- D-Bus names -----------------------------------------------------------

const WPA_SERVICE: &str = "fi.w1.wpa_supplicant1";
const WPA_ROOT_PATH: &str = "/fi/w1/wpa_supplicant1";
const IFACE_ROOT: &str = "fi.w1.wpa_supplicant1";
const IFACE_INTERFACE: &str = "fi.w1.wpa_supplicant1.Interface";
const IFACE_P2PDEVICE: &str = "fi.w1.wpa_supplicant1.Interface.P2PDevice";
const IFACE_WPS: &str = "fi.w1.wpa_supplicant1.Interface.WPS";
const IFACE_PEER: &str = "fi.w1.wpa_supplicant1.Peer";

// Matches cli.rs WFD subelement constants (byte-for-byte payload). These are the
// hex forms in cli.rs; here we decode them to raw bytes for the WFDIEs `ay`.
const WFD_ASSOCIATED_BSSID_SUBELEMENT_HEX: &str = "0006000000000000";
const WFD_COUPLED_SINK_SUBELEMENT_HEX: &str = "000700000000000000";

const WPS_REARM_INTERVAL: Duration = Duration::from_secs(90);
const GROUP_STARTED_TIMEOUT: Duration = Duration::from_secs(10);
/// Short blocking read used by the monitor loop so `running` can be re-checked.
const SIGNAL_POLL_TIMEOUT: Duration = Duration::from_secs(1);

/// The D-Bus backend. Holds a system-bus connection and the resolved P2P device
/// interface object path (set on first `ensure_interface`).
pub struct DbusBackend {
    conn: Connection,
    /// Resolved P2P device interface: (ifname, object path).
    device_iface: std::sync::Mutex<Option<(String, OwnedObjectPath)>>,
}

impl DbusBackend {
    pub fn new() -> BackendResult<Self> {
        let conn = Connection::system()
            .map_err(|e| BackendError::Runtime(format!("D-Bus system bus connect failed: {e}")))?;
        Ok(Self {
            conn,
            device_iface: std::sync::Mutex::new(None),
        })
    }

    /// Build a proxy on the wpa_supplicant service at `path` for `interface`.
    fn proxy<'a>(&self, path: &'a str, interface: &'a str) -> BackendResult<Proxy<'a>> {
        Proxy::new(&self.conn, WPA_SERVICE, path, interface)
            .map_err(|e| BackendError::Runtime(format!("D-Bus proxy ({interface}) failed: {e}")))
    }

    /// Build a proxy for an object addressed by an owned object path.
    fn proxy_at(&self, path: &OwnedObjectPath, interface: &str) -> BackendResult<Proxy<'_>> {
        // Proxy::new needs a path string; ObjectPath derefs to &str.
        let path_str = path.as_str().to_string();
        Proxy::new(
            &self.conn,
            WPA_SERVICE.to_string(),
            path_str,
            interface.to_string(),
        )
        .map_err(|e| BackendError::Runtime(format!("D-Bus proxy ({interface}) failed: {e}")))
    }

    /// Read the `Ifname` (string) property of an Interface object.
    fn ifname_of(&self, iface_obj: &OwnedObjectPath) -> BackendResult<String> {
        let p = self.proxy_at(iface_obj, IFACE_INTERFACE)?;
        let name: String = p
            .get_property("Ifname")
            .map_err(|e| BackendError::Runtime(format!("read Ifname failed: {e}")))?;
        Ok(name)
    }

    /// Find the Interface object path whose `Ifname` == `want`.
    fn find_interface_by_name(&self, want: &str) -> BackendResult<OwnedObjectPath> {
        let root = self.proxy(WPA_ROOT_PATH, IFACE_ROOT)?;
        let interfaces: Vec<OwnedObjectPath> = root
            .get_property("Interfaces")
            .map_err(|e| BackendError::Runtime(format!("read Interfaces failed: {e}")))?;
        for obj in interfaces {
            if let Ok(name) = self.ifname_of(&obj) {
                if name == want {
                    return Ok(obj);
                }
            }
        }
        Err(BackendError::Runtime(format!(
            "no wpa_supplicant interface named {want}"
        )))
    }

    fn cached_device(&self) -> Option<(String, OwnedObjectPath)> {
        self.device_iface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Resolve + cache the parent P2P device interface object path.
    fn ensure_device_obj(&self) -> BackendResult<(String, OwnedObjectPath)> {
        if let Some(d) = self.cached_device() {
            return Ok(d);
        }
        let root = self.proxy(WPA_ROOT_PATH, IFACE_ROOT)?;
        let interfaces: Vec<OwnedObjectPath> = root
            .get_property("Interfaces")
            .map_err(|e| BackendError::Runtime(format!("read Interfaces failed: {e}")))?;
        let first = interfaces
            .into_iter()
            .next()
            .ok_or_else(|| BackendError::Runtime("no wpa_supplicant interfaces present".into()))?;
        let name = self.ifname_of(&first)?;
        let resolved = (name.clone(), first);
        *self.device_iface.lock().unwrap_or_else(|e| e.into_inner()) = Some(resolved.clone());
        Ok(resolved)
    }
}

impl P2pBackend for DbusBackend {
    fn ensure_interface(&self) -> BackendResult<String> {
        let (name, _) = self.ensure_device_obj()?;
        Ok(name)
    }

    fn start_group_owner(&self, device_name: &str, rtsp_port: u16) -> BackendResult<String> {
        let (iface, dev_obj) = self.ensure_device_obj()?;
        log::info!("Setting up P2P GO on {iface}");

        // (a) WFDIEs on the ROOT object — hex-decode of the exact CLI payload:
        //     0006 0011 <rtsp_port BE> 012C  ++ associated-BSSID ++ coupled-sink.
        let wfd_ies = build_wfd_ies(rtsp_port)?;
        let root = self.proxy(WPA_ROOT_PATH, IFACE_ROOT)?;
        root.set_property::<Vec<u8>>("WFDIEs", wfd_ies)
            .map_err(|e| BackendError::Runtime(format!("set WFDIEs failed: {e}")))?;
        log::debug!("WFD subelements configured");

        // (b) P2PDeviceConfig (a{sv}) on the P2PDevice interface of our device.
        let p2p = self.proxy_at(&dev_obj, IFACE_P2PDEVICE)?;
        let mut cfg: HashMap<&str, Value> = HashMap::new();
        cfg.insert("DeviceName", Value::from(device_name.to_string()));
        cfg.insert("GOIntent", Value::from(15u32));
        p2p.set_property::<HashMap<&str, Value>>("P2PDeviceConfig", cfg)
            .map_err(|e| BackendError::Runtime(format!("set P2PDeviceConfig failed: {e}")))?;

        // (c) Find({DiscoveryType: "progressive"}).
        let mut find_args: HashMap<&str, Value> = HashMap::new();
        find_args.insert("DiscoveryType", Value::from("progressive"));
        p2p.call_method("Find", &(find_args,))
            .map_err(|e| BackendError::Runtime(format!("P2PDevice.Find failed: {e}")))?;
        log::debug!("P2P find started (advertising WFD IEs)");

        // (d) GroupAdd — autonomous GO.
        //
        // NOTE: the wpa_cli *command* `p2p_group_add persistent` takes a
        // "persistent" word, but the D-Bus `GroupAdd` method on wpa_supplicant
        // 2.10 REJECTS a `persistent` key (and a `freq` key) with
        // `InvalidArgs: Did not receive correct message arguments`. An EMPTY
        // a{sv} is accepted and creates an autonomous GO — verified live against
        // wpasupplicant 2.10-21ubuntu0.4 (a new p2p-* interface appears). So
        // send an empty options dict.
        let add_args: HashMap<&str, Value> = HashMap::new();
        p2p.call_method("GroupAdd", &(add_args,))
            .map_err(|e| BackendError::Runtime(format!("P2PDevice.GroupAdd failed: {e}")))?;
        log::info!("p2p_group_add issued, waiting for group interface...");

        // (e) Wait for the group interface to appear. The `GroupStarted` D-Bus
        //     signal is unreliable on some wpa_supplicant/NM setups (and a
        //     blocking signal `next()` has no timeout — it froze the GTK main
        //     thread here), so poll `ip link` for the `p2p-*` interface with a
        //     hard deadline, reusing the CLI backend's proven parser. This is a
        //     bounded read; it never blocks past GROUP_STARTED_TIMEOUT.
        let deadline = Instant::now() + GROUP_STARTED_TIMEOUT;
        loop {
            if let Ok(out) = std::process::Command::new("ip")
                .args(["link", "show"])
                .output()
            {
                if out.status.success() {
                    if let Some(group_iface) =
                        cli::parse_group_interface(&String::from_utf8_lossy(&out.stdout))
                    {
                        log::info!("P2P GO created on interface: {group_iface}");
                        return Ok(group_iface);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(BackendError::Runtime(
                    "P2P group interface did not appear within 10 seconds".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn remove_group(&self, group_interface: &str) -> BackendResult<()> {
        // Find the interface whose Ifname matches, then Disconnect its P2PDevice.
        let iface_obj = match self.find_interface_by_name(group_interface) {
            Ok(o) => o,
            Err(e) => {
                // Benign — group may already be gone (mirror cli.rs tolerance).
                log::info!(
                    "Removed P2P group on {group_interface} (interface already absent: {e})"
                );
                return Ok(());
            }
        };
        let p2p = self.proxy_at(&iface_obj, IFACE_P2PDEVICE)?;
        match p2p.call_method("Disconnect", &()) {
            Ok(_) => {
                log::info!("Removed P2P group on {group_interface}");
                Ok(())
            }
            Err(e) => {
                // wpa_supplicant returns an error if there is no active group;
                // treat as benign like the CLI's tolerant p2p_group_remove.
                log::info!("Removed P2P group on {group_interface} (Disconnect benign error: {e})");
                Ok(())
            }
        }
    }

    fn arm_wps_pin(&self, group_interface: &str, pin: &str) -> BackendResult<()> {
        // Retry up to 10× with 1s delays — the group interface's WPS object may
        // not be registered on the bus immediately after GroupAdd.
        for attempt in 0..10 {
            match self.arm_wps_pin_once(group_interface, pin) {
                Ok(()) => {
                    log::info!(
                        "WPS PIN armed: {pin} on {group_interface} (attempt {})",
                        attempt + 1
                    );
                    return Ok(());
                }
                Err(e) => log::debug!("wps_pin attempt {} error: {e}", attempt + 1),
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        log::error!("Failed to arm WPS PIN after 10 attempts");
        Err(BackendError::Runtime(
            "Failed to arm WPS PIN — group interface not ready".to_string(),
        ))
    }

    fn run_event_monitor(&self, group_interface: &str, tx: Sender<P2pEvent>, running: &AtomicBool) {
        log::info!("Event monitor starting on group interface {group_interface}");

        // Resolve the group interface object; retry briefly since GroupStarted
        // and GroupAdd completion can race the interface's bus registration.
        let iface_obj = {
            let mut found = None;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(o) = self.find_interface_by_name(group_interface) {
                    found = Some(o);
                    break;
                }
                if !running.load(Ordering::SeqCst) {
                    log::info!("Event monitor thread exiting");
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            match found {
                Some(o) => o,
                None => {
                    let msg = format!("group interface {group_interface} not found on D-Bus");
                    log::error!("Failed to start event monitor: {msg}");
                    let _ = tx.send(P2pEvent::Error(msg));
                    return;
                }
            }
        };

        // Own a fresh system-bus connection for this worker thread so the
        // blocking signal iterators do not contend with the main-loop
        // connection. (`self.conn` is used from the advertiser thread.)
        let conn = match Connection::system() {
            Ok(c) => c,
            Err(e) => {
                log::error!("Failed to start event monitor: {e}");
                let _ = tx.send(P2pEvent::Error(format!(
                    "Failed to start event monitor: {e}"
                )));
                return;
            }
        };
        // PeerJoined / PeerDisconnected / GroupFinished are emitted on the
        // P2PDevice interface of the GROUP interface object.
        let p2p = match Proxy::new(
            &conn,
            WPA_SERVICE.to_string(),
            iface_obj.as_str().to_string(),
            IFACE_P2PDEVICE.to_string(),
        ) {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to start event monitor: {e}");
                let _ = tx.send(P2pEvent::Error(format!(
                    "Failed to start event monitor: {e}"
                )));
                return;
            }
        };

        // The zbus 4 blocking `MessageIterator` has no timeout / non-blocking
        // `next()`, so it cannot be polled against `running` directly. We move
        // the three blocking iterators onto dedicated reader threads that
        // forward each signal into an mpsc channel; the main loop then does a
        // bounded `recv_timeout`, which lets it re-check `running` and fire the
        // periodic WPS re-arm without ever blocking indefinitely.
        let (sig_tx, sig_rx) = std::sync::mpsc::channel::<SignalMsg>();

        macro_rules! spawn_reader {
            ($name:expr, $kind:expr) => {{
                let mut iter = match p2p.receive_signal($name) {
                    Ok(s) => s,
                    Err(e) => {
                        let m = format!("subscribe {} failed: {e}", $name);
                        log::error!("Failed to start event monitor: {m}");
                        let _ = tx.send(P2pEvent::Error(m));
                        return;
                    }
                };
                let fwd = sig_tx.clone();
                std::thread::spawn(move || {
                    // Ends when the connection/proxy is dropped and `next()`
                    // returns None, or when the receiver is gone (send errors).
                    while let Some(msg) = iter.next() {
                        if fwd.send(($kind, msg)).is_err() {
                            break;
                        }
                    }
                });
            }};
        }
        spawn_reader!("PeerJoined", SignalKind::PeerJoined);
        spawn_reader!("PeerDisconnected", SignalKind::PeerDisconnected);
        spawn_reader!("GroupFinished", SignalKind::GroupFinished);
        drop(sig_tx); // only the reader threads hold senders now

        log::info!("Event monitor attached to {group_interface} — waiting for connections");

        let mut last_rearm = Instant::now();
        while running.load(Ordering::SeqCst) {
            // Periodic WPS re-arm signal to the handler (handler owns the PIN).
            if last_rearm.elapsed() >= WPS_REARM_INTERVAL {
                let _ = tx.send(P2pEvent::WpsPinNeeded);
                last_rearm = Instant::now();
            }

            match sig_rx.recv_timeout(SIGNAL_POLL_TIMEOUT) {
                Ok((SignalKind::GroupFinished, _msg)) => {
                    log::info!("P2P group removed");
                    let _ = tx.send(P2pEvent::GroupRemoved);
                    break;
                }
                Ok((SignalKind::PeerJoined, msg)) => match self.peer_mac_from_signal(&msg) {
                    Ok(mac) => {
                        log::info!("GO event: PeerJoined {mac}");
                        let _ = tx.send(P2pEvent::PeerConnected { mac });
                    }
                    // cli.rs only emits a connect with a MAC; skip if unreadable.
                    Err(e) => log::debug!("PeerJoined without usable Address: {e}"),
                },
                Ok((SignalKind::PeerDisconnected, _msg)) => {
                    log::info!("GO event: PeerDisconnected");
                    let _ = tx.send(P2pEvent::PeerDisconnected);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // No signal this slice — loop to re-check `running` / re-arm.
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // All reader threads ended (connection dropped / bus error).
                    let _ = tx.send(P2pEvent::Error(
                        "D-Bus signal stream ended unexpectedly".to_string(),
                    ));
                    break;
                }
            }
        }

        log::info!("Event monitor thread exiting");
    }
}

/// Which group signal a reader thread forwarded.
#[derive(Debug, Clone, Copy)]
enum SignalKind {
    PeerJoined,
    PeerDisconnected,
    GroupFinished,
}

type SignalMsg = (SignalKind, zbus::Message);

impl DbusBackend {
    /// One WPS.Start attempt on the group interface's WPS object.
    fn arm_wps_pin_once(&self, group_interface: &str, pin: &str) -> BackendResult<()> {
        let iface_obj = self.find_interface_by_name(group_interface)?;
        let wps = self.proxy_at(&iface_obj, IFACE_WPS)?;
        let mut args: HashMap<&str, Value> = HashMap::new();
        args.insert("Role", Value::from("registrar"));
        args.insert("Type", Value::from("pin"));
        args.insert("Pin", Value::from(pin.to_string()));
        wps.call_method("Start", &(args,))
            .map_err(|e| BackendError::Runtime(format!("WPS.Start failed: {e}")))?;
        Ok(())
    }

    /// Extract the peer MAC from a PeerJoined signal.
    ///
    /// PeerJoined carries a single object path (`o`) to the Peer. We read that
    /// Peer object's `DeviceAddress` (`ay`, 6 bytes) and format it as lowercase
    /// `xx:xx:xx:xx:xx:xx`.
    fn peer_mac_from_signal(&self, msg: &zbus::Message) -> BackendResult<String> {
        let body = msg.body();
        let peer_path: OwnedObjectPath = body
            .deserialize()
            .map_err(|e| BackendError::Runtime(format!("PeerJoined body parse failed: {e}")))?;
        let peer = self.proxy_at(&peer_path, IFACE_PEER)?;
        // wpa_supplicant exposes the peer's MAC as `DeviceAddress` (ay).
        let addr: Vec<u8> = peer
            .get_property("DeviceAddress")
            .map_err(|e| BackendError::Runtime(format!("read peer DeviceAddress failed: {e}")))?;
        if addr.len() != 6 {
            return Err(BackendError::Runtime(format!(
                "peer DeviceAddress has {} bytes, expected 6",
                addr.len()
            )));
        }
        Ok(format_mac(&addr))
    }
}

// ---- Free helpers ----------------------------------------------------------

/// Decode an even-length hex string to bytes. Returns `Value`/Runtime error on
/// malformed input (should never happen for compile-time constants).
fn hex_decode(s: &str) -> BackendResult<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(BackendError::Value(format!("odd-length hex: {s}")));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> BackendResult<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(BackendError::Value(format!("bad hex byte: {}", b as char))),
    }
}

/// Assemble the raw `WFDIEs` byte array: hex-decode of
/// `cli::encode_wfd_device_info(port)` (device-info subelement) followed by the
/// associated-BSSID and coupled-sink subelements — byte-for-byte the CLI set.
fn build_wfd_ies(rtsp_port: u16) -> BackendResult<Vec<u8>> {
    let mut ies = hex_decode(&cli::encode_wfd_device_info(rtsp_port))?;
    ies.extend_from_slice(&hex_decode(WFD_ASSOCIATED_BSSID_SUBELEMENT_HEX)?);
    ies.extend_from_slice(&hex_decode(WFD_COUPLED_SINK_SUBELEMENT_HEX)?);
    Ok(ies)
}

/// Format 6 MAC bytes as lowercase `xx:xx:xx:xx:xx:xx`.
fn format_mac(addr: &[u8]) -> String {
    addr.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wfd_ies_device_info_prefix_for_7236() {
        // cli::encode_wfd_device_info(7236) == "000600111C44012C"
        // -> bytes: 00 06 00 11 1C 44 01 2C
        let ies = build_wfd_ies(7236).expect("build");
        assert_eq!(
            &ies[0..8],
            &[0x00, 0x06, 0x00, 0x11, 0x1C, 0x44, 0x01, 0x2C]
        );
    }

    #[test]
    fn wfd_ies_full_assembly_matches_cli_subelements() {
        let ies = build_wfd_ies(7236).expect("build");
        // device-info (8) + associated-bssid (8) + coupled-sink (9) = 25 bytes.
        assert_eq!(ies.len(), 8 + 8 + 9);
        // associated-BSSID subelement: 00 06 00 00 00 00 00 00
        assert_eq!(
            &ies[8..16],
            &[0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        // coupled-sink subelement: 00 07 00 00 00 00 00 00 00
        assert_eq!(
            &ies[16..25],
            &[0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn hex_decode_roundtrip() {
        assert_eq!(
            hex_decode("000600111C44012C").unwrap(),
            vec![0x00, 0x06, 0x00, 0x11, 0x1C, 0x44, 0x01, 0x2C]
        );
        assert!(hex_decode("ABC").is_err()); // odd length
        assert!(hex_decode("ZZ").is_err()); // bad nibble
    }

    #[test]
    fn mac_formats_lowercase_colon_separated() {
        assert_eq!(
            format_mac(&[0xAA, 0xBB, 0xCC, 0x00, 0x11, 0x2F]),
            "aa:bb:cc:00:11:2f"
        );
    }
}
