//! WFD Sink Advertiser for Ubuntu Miracast Server.
//!
//! Uses the Autonomous Group Owner approach: creates a P2P GO first, then
//! arms WPS PIN on the group interface (done in `connection`). This is the
//! proven approach used by lazycast and 7herbert.
//!
//! The actual P2P control plane is behind [`P2pBackend`] (wpa_cli subprocess by
//! default, wpa_supplicant D-Bus when selected) — this module owns the
//! advertising lifecycle + event emission and delegates the control calls.

use crate::events::{Event, EventSender};
use crate::p2p_backend::P2pBackend;
use crate::sync_ext::LockExt;
use std::sync::{Arc, Mutex};

/// Manages WFD sink advertising via an Autonomous P2P Group Owner.
pub struct MiracastAdvertiser {
    device_name: String,
    rtsp_port: u16,
    p2p_interface: Option<String>,
    group_interface: Arc<Mutex<Option<String>>>,
    advertising: Arc<Mutex<bool>>,
    events: EventSender,
    backend: Arc<dyn P2pBackend>,
}

impl MiracastAdvertiser {
    pub fn new(
        device_name: impl Into<String>,
        rtsp_port: u16,
        p2p_interface: Option<String>,
        backend: Arc<dyn P2pBackend>,
        events: EventSender,
    ) -> Self {
        Self {
            device_name: device_name.into(),
            rtsp_port,
            p2p_interface,
            group_interface: Arc::new(Mutex::new(None)),
            advertising: Arc::new(Mutex::new(false)),
            events,
            backend,
        }
    }

    pub fn is_advertising(&self) -> bool {
        *self.advertising.lock_safe()
    }

    pub fn p2p_interface(&self) -> Option<String> {
        // Prefer the backend's resolved interface once known.
        self.backend
            .ensure_interface()
            .ok()
            .or_else(|| self.p2p_interface.clone())
    }

    pub fn group_interface(&self) -> Option<String> {
        self.group_interface.lock_safe().clone()
    }

    /// The backend handle, so the connection handler shares the same one.
    pub fn backend(&self) -> Arc<dyn P2pBackend> {
        Arc::clone(&self.backend)
    }

    /// Allow the app to update the interface at runtime (interface switching).
    pub fn set_p2p_interface(&mut self, iface: impl Into<String>) {
        self.p2p_interface = Some(iface.into());
    }

    /// Start WFD sink advertising by creating an Autonomous P2P Group Owner.
    pub fn start_advertising(&mut self) {
        {
            let advertising = self.advertising.lock_safe();
            if *advertising {
                log::debug!("Already advertising — ignoring");
                return;
            }
        }

        match self
            .backend
            .start_group_owner(&self.device_name, self.rtsp_port)
        {
            Ok(group_iface) => {
                *self.group_interface.lock_safe() = Some(group_iface.clone());
                *self.advertising.lock_safe() = true;
                let _ = self.events.send(Event::AdvertisingStarted {
                    group_interface: group_iface.clone(),
                });
                log::info!(
                    "Advertising as '{}' via GO on {} (RTSP port {})",
                    self.device_name,
                    group_iface,
                    self.rtsp_port
                );
            }
            Err(e) => {
                let error_msg = format!("Failed to start advertising: {e}");
                log::error!("{error_msg}");
                let _ = self.events.send(Event::AdvertisingError(error_msg));
            }
        }
    }

    /// Stop advertising by removing the P2P group.
    pub fn stop_advertising(&mut self) {
        {
            let mut advertising = self.advertising.lock_safe();
            if !*advertising {
                return;
            }
            *advertising = false;
        }

        let group = self.group_interface.lock_safe().clone();
        if let Some(group_iface) = group {
            match self.backend.remove_group(&group_iface) {
                Ok(()) => log::info!("Removed P2P group on {group_iface}"),
                Err(e) => log::warn!("Error removing P2P group: {e}"),
            }
            *self.group_interface.lock_safe() = None;
        }
        let _ = self.events.send(Event::AdvertisingStopped);
    }
}
