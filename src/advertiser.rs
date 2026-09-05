//! WFD Sink Advertiser for Ubuntu Miracast Server.
//!
//! Uses the Autonomous Group Owner approach: creates a P2P GO first, then
//! arms WPS PIN on the group interface (done in `connection`). This is the
//! proven approach used by lazycast and 7herbert.
//!
//! Faithful port of `src/miracast_server/advertiser.py`.

use crate::events::{Event, EventSender};
use crate::utils::{find_p2p_interface, run_wpa_cli, WpaError};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// WFD Device Info: Primary Sink (01) + Session Available (10) = 0x0011
// (matching lazycast's proven working value — no WSD bit).
const WFD_ASSOCIATED_BSSID_SUBELEMENT: &str = "0006000000000000";
const WFD_COUPLED_SINK_SUBELEMENT: &str = "000700000000000000";

/// Encode WFD Device Information sub-element for a Primary Sink.
///
/// Byte-exact with the Python `_encode_wfd_device_info`:
/// `0006` + DevInfo(0011) + rtsp_port(hex) + throughput(012C).
fn encode_wfd_device_info(rtsp_port: u16) -> String {
    let device_info: u16 = 0x0011; // Primary Sink + Session Available
    let throughput: u16 = 0x012C; // 300 Mbps
    format!("0006{device_info:04X}{rtsp_port:04X}{throughput:04X}")
}

/// Manages WFD sink advertising via an Autonomous P2P Group Owner.
pub struct MiracastAdvertiser {
    device_name: String,
    rtsp_port: u16,
    p2p_interface: Option<String>,
    ctrl_path: Option<String>,
    group_interface: Arc<Mutex<Option<String>>>,
    advertising: Arc<Mutex<bool>>,
    events: EventSender,
}

impl MiracastAdvertiser {
    pub fn new(
        device_name: impl Into<String>,
        rtsp_port: u16,
        p2p_interface: Option<String>,
        ctrl_path: Option<String>,
        events: EventSender,
    ) -> Self {
        Self {
            device_name: device_name.into(),
            rtsp_port,
            p2p_interface,
            ctrl_path,
            group_interface: Arc::new(Mutex::new(None)),
            advertising: Arc::new(Mutex::new(false)),
            events,
        }
    }

    pub fn is_advertising(&self) -> bool {
        *self.advertising.lock().unwrap()
    }

    pub fn p2p_interface(&self) -> Option<String> {
        self.p2p_interface.clone()
    }

    pub fn ctrl_path(&self) -> Option<String> {
        self.ctrl_path.clone()
    }

    pub fn group_interface(&self) -> Option<String> {
        self.group_interface.lock().unwrap().clone()
    }

    /// Allow the app to update the interface at runtime (interface switching).
    pub fn set_p2p_interface(&mut self, iface: impl Into<String>) {
        self.p2p_interface = Some(iface.into());
    }

    /// Run wpa_cli on the given (or default) interface with our ctrl_path.
    fn wpa(
        &self,
        args: &[&str],
        interface: Option<&str>,
        skip_last_validation: bool,
    ) -> Result<String, WpaError> {
        let iface = interface
            .map(|s| s.to_string())
            .or_else(|| self.p2p_interface.clone())
            .unwrap_or_default();
        run_wpa_cli(
            &iface,
            args,
            skip_last_validation,
            self.ctrl_path.as_deref(),
        )
    }

    /// Start WFD sink advertising by creating an Autonomous P2P Group Owner.
    pub fn start_advertising(&mut self) {
        {
            let advertising = self.advertising.lock().unwrap();
            if *advertising {
                log::debug!("Already advertising — ignoring");
                return;
            }
        }

        if let Err(e) = self.start_advertising_inner() {
            let error_msg = format!("Failed to start advertising: {e}");
            log::error!("{error_msg}");
            let _ = self.events.send(Event::AdvertisingError(error_msg));
        }
    }

    fn start_advertising_inner(&mut self) -> Result<(), WpaError> {
        // Step 1: Resolve interface.
        if self.p2p_interface.is_none() {
            let (p2p_iface, _) = find_p2p_interface()?;
            self.p2p_interface = Some(p2p_iface);
        }
        let iface = self.p2p_interface.clone().unwrap();
        log::info!("Setting up P2P GO on {iface}");

        // Step 2: Enable WFD and set subelements (identical argv + order).
        let dev_info = encode_wfd_device_info(self.rtsp_port);
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
        self.wpa(&["set", "device_name", &self.device_name], None, true)?;
        self.wpa(&["set", "device_type", "7-0050F204-1"], None, false)?;
        self.wpa(&["set", "p2p_go_ht40", "1"], None, false)?;
        log::debug!("WFD subelements configured");

        // Step 3: Start P2P find (makes WFD IEs visible to Miracast sources).
        self.wpa(&["p2p_find", "type=progressive"], None, true)?;
        log::debug!("P2P find started (advertising WFD IEs)");

        // Step 4: Create Autonomous P2P Group Owner.
        let result = self.wpa(&["p2p_group_add", "persistent"], None, false)?;
        if result.contains("FAIL") {
            return Err(WpaError::Runtime(format!("p2p_group_add failed: {result}")));
        }
        log::info!("p2p_group_add issued, waiting for group interface...");

        // Step 4b: Wait for group interface to appear.
        let group_iface = wait_for_group_interface(Duration::from_secs(10)).ok_or_else(|| {
            WpaError::Runtime("P2P group interface did not appear within 10 seconds".to_string())
        })?;
        *self.group_interface.lock().unwrap() = Some(group_iface.clone());
        log::info!("P2P GO created on interface: {group_iface}");

        // Step 5: Set WFD subelements on the group interface too (best-effort).
        if let Err(e) = self.wpa(&["set", "wifi_display", "1"], Some(&group_iface), false) {
            log::debug!("Could not set WFD on group iface (may not be needed): {e}");
        } else if let Err(e) = self.wpa(
            &["wfd_subelem_set", "0", &dev_info],
            Some(&group_iface),
            false,
        ) {
            log::debug!("Could not set WFD on group iface (may not be needed): {e}");
        }

        *self.advertising.lock().unwrap() = true;
        let _ = self.events.send(Event::AdvertisingStarted {
            group_interface: group_iface.clone(),
        });
        log::info!(
            "Advertising as '{}' via GO on {} (RTSP port {})",
            self.device_name,
            group_iface,
            self.rtsp_port
        );
        Ok(())
    }

    /// Stop advertising by removing the P2P group.
    pub fn stop_advertising(&mut self) {
        {
            let mut advertising = self.advertising.lock().unwrap();
            if !*advertising {
                return;
            }
            *advertising = false;
        }

        let group = self.group_interface.lock().unwrap().clone();
        if let Some(group_iface) = group {
            match self.wpa(&["p2p_group_remove", &group_iface], None, false) {
                Ok(_) => log::info!("Removed P2P group on {group_iface}"),
                Err(e) => log::warn!("Error removing P2P group: {e}"),
            }
            *self.group_interface.lock().unwrap() = None;
        }
        let _ = self.events.send(Event::AdvertisingStopped);
    }
}

/// Wait for the P2P group interface to appear after `p2p_group_add`.
/// Polls `ip link show` for a `p2p-*` interface (identical to the Python).
fn wait_for_group_interface(timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(out) = Command::new("ip").args(["link", "show"]).output() {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if line.contains(": p2p-") {
                        // "<idx>: p2p-...@parent: ..." → take the iface name.
                        if let Some((_, rest)) = line.split_once(": ") {
                            let iface = rest.split('@').next().unwrap_or(rest);
                            let iface = iface.trim_end_matches(':');
                            return Some(iface.to_string());
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_subelement_hex_is_exact() {
        // Standard WFD port 7236 = 0x1C44 → matches the Python constant
        // 000600111C44012C.
        assert_eq!(encode_wfd_device_info(7236), "000600111C44012C");
    }

    #[test]
    fn device_info_subelement_other_port() {
        // 0x1C45 = 7237.
        assert_eq!(encode_wfd_device_info(7237), "000600111C45012C");
    }

    #[test]
    fn subelement_constants_match_python() {
        assert_eq!(WFD_ASSOCIATED_BSSID_SUBELEMENT, "0006000000000000");
        assert_eq!(WFD_COUPLED_SINK_SUBELEMENT, "000700000000000000");
    }
}
