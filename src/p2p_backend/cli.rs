//! `wpa_cli` (subprocess) implementation of [`P2pBackend`].
//!
//! This is the DEFAULT backend and is a faithful relocation of the logic that
//! previously lived inline in `advertiser.rs` / `connection.rs` — identical
//! argv, identical WFD subelement hex, identical ATTACH event loop. It is the
//! hardware-validated path; the D-Bus backend is opt-in.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use regex::Regex;

use super::{BackendResult, P2pBackend, P2pEvent};
use crate::utils::{find_p2p_interface, run_wpa_cli, WpaError};

// WFD Device Info: Primary Sink (01) + Session Available (10) = 0x0011
// (matching lazycast's proven working value — no WSD bit).
const WFD_ASSOCIATED_BSSID_SUBELEMENT: &str = "0006000000000000";
const WFD_COUPLED_SINK_SUBELEMENT: &str = "000700000000000000";

const WPS_REARM_INTERVAL: Duration = Duration::from_secs(240);

/// WPS PIN validity window passed to `wps_pin any <pin> <timeout>`. The
/// wpa_supplicant default is 120s, too short to comfortably read an 8-digit PIN
/// and type it on a phone. 300s gives a generous window; the rearm interval
/// (240s) sits just inside it so the SAME PIN is continuously re-armed and never
/// lapses between rearms.
const WPS_PIN_TIMEOUT_SECS: &str = "300";

/// Encode WFD Device Information sub-element for a Primary Sink.
/// Byte-exact: `0006` + DevInfo(0011) + rtsp_port(hex) + throughput(012C).
pub(crate) fn encode_wfd_device_info(rtsp_port: u16) -> String {
    let device_info: u16 = 0x0011;
    let throughput: u16 = 0x012C;
    format!("0006{device_info:04X}{rtsp_port:04X}{throughput:04X}")
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

/// The subprocess backend. Holds an optional dedicated-supplicant control path
/// and the resolved P2P device interface.
pub struct WpaCliBackend {
    ctrl_path: Option<String>,
    /// Resolved P2P device interface (set on first `ensure_interface`).
    p2p_interface: std::sync::Mutex<Option<String>>,
    /// Ordered GO bring-up ladder (capability-detected). Empty = single default.
    go_candidates: Vec<crate::capabilities::GoCandidate>,
    /// Shared cell the receiver reads for its advertised resolution; set to the
    /// winning candidate's resolution once a GO comes up.
    won_resolution: std::sync::Arc<std::sync::Mutex<(u32, u32)>>,
    /// Phase-2 discovery rotation: how many social rungs to skip at the front of
    /// the ladder on the NEXT bring-up (advanced by rotate_discovery_channel).
    rotation_offset: std::sync::atomic::AtomicUsize,
}

impl WpaCliBackend {
    pub fn new(ctrl_path: Option<String>) -> Self {
        Self {
            ctrl_path,
            p2p_interface: std::sync::Mutex::new(None),
            go_candidates: Vec::new(),
            won_resolution: std::sync::Arc::new(std::sync::Mutex::new((1280, 720))),
            rotation_offset: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Seed the capability-detected GO bring-up ladder + the shared resolution
    /// cell updated to the winning rung's resolution.
    pub fn with_go_candidates(
        mut self,
        candidates: Vec<crate::capabilities::GoCandidate>,
        won_resolution: std::sync::Arc<std::sync::Mutex<(u32, u32)>>,
    ) -> Self {
        self.go_candidates = candidates;
        self.won_resolution = won_resolution;
        self
    }

    /// Optionally seed the P2P interface (from config / CLI flag).
    pub fn with_interface(self, iface: Option<String>) -> Self {
        *self.p2p_interface.lock().unwrap_or_else(|e| e.into_inner()) = iface;
        self
    }

    fn wpa(
        &self,
        args: &[&str],
        interface: Option<&str>,
        skip_last_validation: bool,
    ) -> Result<String, WpaError> {
        let iface = interface
            .map(|s| s.to_string())
            .or_else(|| self.current_iface())
            .unwrap_or_default();
        run_wpa_cli(
            &iface,
            args,
            skip_last_validation,
            self.ctrl_path.as_deref(),
        )
    }

    fn current_iface(&self) -> Option<String> {
        self.p2p_interface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl P2pBackend for WpaCliBackend {
    fn ensure_interface(&self) -> BackendResult<String> {
        if let Some(i) = self.current_iface() {
            return Ok(i);
        }
        let (p2p_iface, _) = find_p2p_interface()?;
        *self.p2p_interface.lock().unwrap_or_else(|e| e.into_inner()) = Some(p2p_iface.clone());
        Ok(p2p_iface)
    }

    fn start_group_owner(&self, device_name: &str, rtsp_port: u16) -> BackendResult<String> {
        let iface = self.ensure_interface()?;
        log::info!("Setting up P2P GO on {iface}");

        // WFD + device config (identical argv + order to the Python source).
        let dev_info = encode_wfd_device_info(rtsp_port);
        self.wpa(&["set", "wifi_display", "1"], None, false)?;
        self.wpa(&["wfd_subelem_set", "0", &dev_info], None, false)?;
        self.wpa(
            &["wfd_subelem_set", "1", WFD_ASSOCIATED_BSSID_SUBELEMENT],
            None,
            false,
        )?;
        self.wpa(
            &["wfd_subelem_set", "6", WFD_COUPLED_SINK_SUBELEMENT],
            None,
            false,
        )?;
        self.wpa(&["set", "device_name", device_name], None, true)?;
        self.wpa(&["set", "device_type", "7-0050F204-1"], None, false)?;
        self.wpa(&["set", "p2p_go_ht40", "1"], None, false)?;
        log::debug!("WFD subelements configured");

        self.wpa(&["p2p_find", "type=progressive"], None, true)?;
        log::debug!("P2P find started (advertising WFD IEs)");

        // Phase 1 — GO bring-up ladder. Walk the capability-detected candidates
        // (2.4GHz social ch1/6/11 → driver-chosen; 5GHz only if opted in), and
        // use the FIRST whose GO actually comes up and reports the expected
        // band. Each rung: snapshot → p2p_group_add freq=… → wait for a NEW
        // p2p-* iface → verify operating freq. On failure, tear the rung down
        // and try the next. Records the winning rung's resolution for the
        // receiver to advertise.
        let candidates = if self.go_candidates.is_empty() {
            // Safe default ladder if none were seeded.
            vec![
                crate::capabilities::GoCandidate {
                    freq_mhz: 2412,
                    band: crate::capabilities::GoBand::Band24,
                    max_resolution: (1280, 720),
                    label: "2.4GHz ch1",
                },
                crate::capabilities::GoCandidate {
                    freq_mhz: 0,
                    band: crate::capabilities::GoBand::Band24,
                    max_resolution: (1280, 720),
                    label: "driver-chosen",
                },
            ]
        } else {
            self.go_candidates.clone()
        };
        // Phase-2 rotation: rotate the ladder left by the current offset so a
        // no-connect rotation starts bring-up on the next social channel.
        let candidates = {
            let off = self
                .rotation_offset
                .load(std::sync::atomic::Ordering::SeqCst)
                % candidates.len().max(1);
            let mut v = candidates;
            v.rotate_left(off);
            v
        };

        let mut group_iface: Option<String> = None;
        for cand in &candidates {
            let pre = snapshot_group_interfaces();
            let args: Vec<&str> = if cand.freq_mhz == 0 {
                vec!["p2p_group_add", "persistent"]
            } else {
                vec![
                    "p2p_group_add",
                    "persistent",
                    Box::leak(format!("freq={}", cand.freq_mhz).into_boxed_str()),
                ]
            };
            let result = self.wpa(&args, None, false)?;
            if result.contains("FAIL") {
                log::warn!("GO rung '{}' rejected by p2p_group_add; next", cand.label);
                continue;
            }
            let Some(iface) = wait_for_group_interface(Duration::from_secs(10), &pre) else {
                log::warn!(
                    "GO rung '{}' — group iface did not appear; next",
                    cand.label
                );
                continue;
            };
            // Verify the operating frequency matches the rung's band (a P2P-GO
            // exposes freq only via `wpa_cli status`, not `iw dev info`).
            let freq_ok = match self.wpa(&["status"], Some(&iface), true) {
                Ok(status) => {
                    let freq = status
                        .lines()
                        .find_map(|l| l.strip_prefix("freq="))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    log::info!("GO rung '{}' up on {iface} (freq={freq} MHz)", cand.label);
                    // Accept if we can't read it; only reject an obvious band
                    // mismatch (asked 2.4GHz, got 5GHz or vice-versa).
                    match cand.band {
                        crate::capabilities::GoBand::Band24 => !freq.starts_with('5'),
                        crate::capabilities::GoBand::Band5 => freq.starts_with('5'),
                    }
                }
                Err(_) => true, // status unreadable → accept, do not thrash
            };
            if !freq_ok {
                log::warn!(
                    "GO rung '{}' came up on the wrong band; tearing down, next",
                    cand.label
                );
                let _ = self.wpa(&["p2p_group_remove", &iface], None, false);
                continue;
            }
            // Winner. Record the resolution the receiver should advertise.
            *self
                .won_resolution
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = cand.max_resolution;
            let (rw, rh) = cand.max_resolution;
            log::info!(
                "GO bring-up succeeded on rung '{}' ({iface}); advertising {rw}x{rh}",
                cand.label
            );
            group_iface = Some(iface);
            break;
        }
        let group_iface = group_iface.ok_or_else(|| {
            super::BackendError::Runtime(
                "GO bring-up failed on all candidate configs (see per-rung warnings)".to_string(),
            )
        })?;
        log::info!("P2P GO created on interface: {group_iface}");

        // Best-effort: set WFD subelements on the group interface too.
        if let Err(e) = self.wpa(&["set", "wifi_display", "1"], Some(&group_iface), false) {
            log::debug!("Could not set WFD on group iface (may not be needed): {e}");
        } else if let Err(e) = self.wpa(
            &["wfd_subelem_set", "0", &dev_info],
            Some(&group_iface),
            false,
        ) {
            log::debug!("Could not set WFD on group iface (may not be needed): {e}");
        }

        // CRITICAL for discovery: an autonomous GO only BEACONS — it does not by
        // itself stay in P2P Listen state. Android finds a sink via P2P device
        // discovery (Probe Request/Response on the social channels) filtered by
        // the WFD IE, NOT by reading the beacon. So keep the device discoverable
        // while the GO runs: Extended Listen Timing makes it periodically enter
        // Listen state and answer probes (avail 200ms every 500ms), and a fresh
        // p2p_find keeps the P2P state machine advertising. Without this the GO
        // shows up as a Wi-Fi network but never in the phone's Cast list.
        //   p2p_ext_listen <availability_ms> <interval_ms>
        if let Err(e) = self.wpa(&["p2p_ext_listen", "200", "500"], None, false) {
            log::warn!("Could not enable extended listen ({e}); discovery may be unreliable");
        }
        if let Err(e) = self.wpa(&["p2p_find", "type=progressive"], None, true) {
            log::debug!("Post-group p2p_find returned: {e}");
        }
        log::info!("Extended-listen discovery active on {group_iface}");

        Ok(group_iface)
    }

    fn remove_group(&self, group_interface: &str) -> BackendResult<()> {
        self.wpa(&["p2p_group_remove", group_interface], None, false)?;
        log::info!("Removed P2P group on {group_interface}");
        Ok(())
    }

    fn arm_wps_pin(&self, group_interface: &str, pin: &str) -> BackendResult<()> {
        // Retry up to 10× with 1s delays — the group control socket may not be
        // ready immediately after p2p_group_add. Arm with an explicit 300s
        // timeout (WPS default is only 120s) so the user has ample time to read
        // the PIN and type it on the phone before it expires.
        for attempt in 0..10 {
            match run_wpa_cli(
                group_interface,
                &["wps_pin", "any", pin, WPS_PIN_TIMEOUT_SECS],
                false,
                self.ctrl_path.as_deref(),
            ) {
                Ok(result) if !result.contains("FAIL") => {
                    log::info!(
                        "WPS PIN armed: {pin} on {group_interface} (attempt {})",
                        attempt + 1
                    );
                    return Ok(());
                }
                Ok(_) => log::debug!("wps_pin attempt {} failed, retrying...", attempt + 1),
                Err(e) => log::debug!("wps_pin attempt {} error: {e}", attempt + 1),
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        log::error!("Failed to arm WPS PIN after 10 attempts");
        Err(super::BackendError::Runtime(
            "Failed to arm WPS PIN — group interface not ready".to_string(),
        ))
    }

    fn run_event_monitor(&self, group_interface: &str, tx: Sender<P2pEvent>, running: &AtomicBool) {
        log::info!("Event monitor starting on group interface {group_interface}");

        let mut cmd = Command::new("sudo");
        cmd.arg("wpa_cli");
        if let Some(p) = &self.ctrl_path {
            cmd.arg("-p").arg(p);
        }
        cmd.arg("-i").arg(group_interface);

        let mut proc = match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to start wpa_cli: {e}");
                let _ = tx.send(P2pEvent::Error(format!(
                    "Failed to start event monitor: {e}"
                )));
                return;
            }
        };

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
            let _ = tx.send(P2pEvent::Error(format!(
                "wpa_cli failed: {}",
                stderr.trim()
            )));
            return;
        }

        let stdout = proc.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);

        drain_banner(&mut reader);
        if let Some(stdin) = proc.stdin.as_mut() {
            let _ = stdin.write_all(b"ATTACH\n");
            let _ = stdin.flush();
        }
        std::thread::sleep(Duration::from_millis(500));
        drain_output(&mut reader, Duration::from_secs(1));

        log::info!("Event monitor attached to {group_interface} — waiting for connections");

        let mut last_rearm = Instant::now();
        while running.load(Ordering::SeqCst) {
            // The monitor re-arms WPS periodically; it signals that intent to
            // the handler via WpsPinNeeded on the interval (the handler owns the
            // PIN + arming so the backend stays stateless about PIN values).
            if last_rearm.elapsed() >= WPS_REARM_INTERVAL {
                let _ = tx.send(P2pEvent::WpsPinNeeded);
                last_rearm = Instant::now();
            }

            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
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
                let _ = tx.send(P2pEvent::PeerConnected {
                    mac: cap[1].to_string(),
                });
                continue;
            }
            if line.contains("WPS-PIN-NEEDED") {
                let _ = tx.send(P2pEvent::WpsPinNeeded);
                continue;
            }
            if ap_sta_disconnected_re().is_match(&line) {
                let _ = tx.send(P2pEvent::PeerDisconnected);
                continue;
            }
            if line.contains("P2P-GROUP-REMOVED") {
                log::info!("P2P group removed");
                let _ = tx.send(P2pEvent::GroupRemoved);
                break;
            }
        }

        if let Some(stdin) = proc.stdin.as_mut() {
            let _ = stdin.write_all(b"QUIT\n");
            let _ = stdin.flush();
        }
        let _ = proc.kill();
        let _ = proc.wait();
        log::info!("Event monitor thread exiting");
    }

    fn rotate_discovery_channel(&self) -> Option<String> {
        // Rotate only across the 2.4GHz SOCIAL rungs (the discoverable ones);
        // rotating onto driver-chosen/5GHz would not help discovery. The budget
        // is the count of social rungs minus the one already tried.
        let social: Vec<&crate::capabilities::GoCandidate> = self
            .go_candidates
            .iter()
            .filter(|c| c.band == crate::capabilities::GoBand::Band24 && c.freq_mhz != 0)
            .collect();
        let social_count = social.len().max(1);
        let next = self
            .rotation_offset
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if next >= social_count {
            // Exhausted the social channels — signal the caller to prompt.
            return None;
        }
        // The rung that will lead the ladder on the next bring-up.
        social.get(next).map(|c| c.label.to_string())
    }
}

/// List ALL current `p2p-*` interface names from `ip link show`.
pub(crate) fn list_group_interfaces(output: &str) -> Vec<String> {
    let mut v = Vec::new();
    for line in output.lines() {
        if line.contains(": p2p-") {
            let parts: Vec<&str> = line.split(": ").collect();
            if parts.len() >= 2 {
                let iface = parts[1].split('@').next().unwrap_or(parts[1]);
                let iface = iface.trim_end_matches(':').trim();
                if !iface.is_empty() {
                    v.push(iface.to_string());
                }
            }
        }
    }
    v
}

/// Snapshot the set of `p2p-*` interfaces that exist RIGHT NOW. Call this
/// immediately before `p2p_group_add` so `wait_for_group_interface` can tell the
/// newly-created group from stale leftovers of a previous run.
fn snapshot_group_interfaces() -> Vec<String> {
    Command::new("ip")
        .args(["link", "show"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| list_group_interfaces(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Wait for a NEWLY-created P2P group interface to appear after
/// `p2p_group_add`. `pre_existing` is the snapshot taken before the call; we
/// return the first `p2p-*` interface NOT in that set. This avoids latching onto
/// a stale group netdev left over from a prior run (which may sit on the wrong
/// radio, e.g. a leftover `p2p-wlo1-*` while we created ours on `wlx`).
fn wait_for_group_interface(timeout: Duration, pre_existing: &[String]) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(out) = Command::new("ip").args(["link", "show"]).output() {
            if out.status.success() {
                let current = list_group_interfaces(&String::from_utf8_lossy(&out.stdout));
                if let Some(new) = current.iter().find(|i| !pre_existing.contains(i)) {
                    return Some(new.clone());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Extract the first `p2p-*` interface name from `ip link show` output.
/// Used by the D-Bus backend's group-interface poll.
#[cfg(feature = "dbus-backend")]
pub(crate) fn parse_group_interface(output: &str) -> Option<String> {
    list_group_interfaces(output).into_iter().next()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_subelement_hex_is_exact() {
        assert_eq!(encode_wfd_device_info(7236), "000600111C44012C");
        assert_eq!(encode_wfd_device_info(7237), "000600111C45012C");
    }

    #[test]
    fn subelement_constants_match_python() {
        assert_eq!(WFD_ASSOCIATED_BSSID_SUBELEMENT, "0006000000000000");
        assert_eq!(WFD_COUPLED_SINK_SUBELEMENT, "000700000000000000");
    }

    #[test]
    fn lists_iface_name_not_the_whole_ip_link_line() {
        let out = "1: lo: <LOOPBACK,UP> mtu 65536\n\
                   3: p2p-0: <NO-CARRIER,BROADCAST,MULTICAST,UP> mtu 1500 qdisc noqueue state DOWN";
        assert_eq!(list_group_interfaces(out), vec!["p2p-0".to_string()]);
    }

    #[test]
    fn lists_iface_name_with_parent_suffix() {
        let out = "5: p2p-wlan0-0@wlan0: <BROADCAST,MULTICAST> mtu 1500";
        assert_eq!(list_group_interfaces(out), vec!["p2p-wlan0-0".to_string()]);
    }

    #[test]
    fn no_p2p_interface_returns_empty() {
        assert!(list_group_interfaces("1: lo: <LOOPBACK,UP>\n2: wlan0: <UP>").is_empty());
    }

    #[test]
    fn new_group_detected_by_diff_ignoring_stale() {
        // Two p2p-* exist; one (p2p-wlo1-8) is a stale leftover present BEFORE
        // group_add. The newly-created one is p2p-wlx...-0. The diff must pick
        // the new one, not the first-listed stale one.
        let before = ["p2p-wlo1-8".to_string()];
        let after = "3: p2p-wlo1-8: <UP> mtu 1500\n\
                     7: p2p-wlx3c78950c6ede-0: <UP> mtu 1500";
        let current = list_group_interfaces(after);
        let new: Vec<_> = current.iter().filter(|i| !before.contains(i)).collect();
        assert_eq!(new, vec![&"p2p-wlx3c78950c6ede-0".to_string()]);
    }
}
