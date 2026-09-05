//! P2P control-plane backend abstraction.
//!
//! The advertiser and connection handler drive Wi-Fi Direct P2P + WFD + WPS
//! through this trait rather than calling `wpa_cli` directly. Two backends
//! implement it:
//!
//! - [`wpa_cli`](crate::p2p_backend::cli) — the original subprocess path
//!   (spawns `sudo wpa_cli`, parses stdout, monitors the interactive ATTACH
//!   stream). This is the DEFAULT and the proven, hardware-validated path.
//! - `dbus` (feature = `dbus-backend`) — talks to `fi.w1.wpa_supplicant1` over
//!   the system bus: sets `WFDIEs` / `P2PDeviceConfig`, calls
//!   `P2PDevice.Find` / `GroupAdd` / `WPS.Start`, and subscribes to
//!   `GroupStarted` / `PeerJoined` / `PeerDisconnected` / `GroupFinished`
//!   signals instead of scraping `wpa_cli` output.
//!
//! Selection is at runtime (see [`select_backend`]): the D-Bus backend is used
//! only when the `dbus-backend` feature is compiled in AND
//! `MIRACAST_BACKEND=dbus` is set (or it is chosen explicitly), so the
//! subprocess path remains the safe default until the D-Bus path is
//! hardware-proven.

use std::sync::mpsc::Sender;

/// A peer connection/disconnection event surfaced by the backend, replacing the
/// `AP-STA-CONNECTED` / `AP-STA-DISCONNECTED` line matching of the CLI monitor.
#[derive(Debug, Clone)]
pub enum P2pEvent {
    /// A source (STA) connected to our GO. Carries its MAC (lowercase hex,
    /// `xx:xx:xx:xx:xx:xx`).
    PeerConnected { mac: String },
    /// A source disconnected.
    PeerDisconnected,
    /// The P2P group was removed / finished.
    GroupRemoved,
    /// A WPS PIN was requested by a peer with no PIN armed (CLI:
    /// `WPS-PIN-NEEDED`) — the handler should re-arm.
    WpsPinNeeded,
    /// A backend-level error worth surfacing.
    Error(String),
}

/// Result type shared by backend operations.
pub type BackendResult<T> = Result<T, BackendError>;

#[derive(Debug, Clone)]
pub enum BackendError {
    /// A parameter was rejected (injection allowlist, bad value).
    Value(String),
    /// The operation failed at runtime (command failed, D-Bus error, timeout).
    Runtime(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Value(m) | BackendError::Runtime(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for BackendError {}

impl From<crate::utils::WpaError> for BackendError {
    fn from(e: crate::utils::WpaError) -> Self {
        match e {
            crate::utils::WpaError::Value(m) => BackendError::Value(m),
            crate::utils::WpaError::Runtime(m) => BackendError::Runtime(m),
        }
    }
}

/// The P2P control-plane operations the app needs. Both the CLI and D-Bus
/// backends implement this; the argv/D-Bus specifics live in the impls.
///
/// Backends must be `Send + Sync` — the advertiser runs on the main loop and
/// the connection monitor runs on a worker thread, both sharing one backend.
pub trait P2pBackend: Send + Sync {
    /// Resolve/confirm the P2P device interface (the parent wl* / p2p-dev-*).
    fn ensure_interface(&self) -> BackendResult<String>;

    /// Configure the WFD sink advertisement + device config and start an
    /// autonomous P2P Group Owner. Returns the created GROUP interface name.
    ///
    /// This is the whole `start_advertising` control sequence: WFD subelements,
    /// device name/type, p2p_go_ht40, p2p_find, p2p_group_add, and waiting for
    /// the group interface to appear.
    fn start_group_owner(&self, device_name: &str, rtsp_port: u16) -> BackendResult<String>;

    /// Remove the P2P group on the given group interface.
    fn remove_group(&self, group_interface: &str) -> BackendResult<()>;

    /// Arm the WPS registrar on the group interface with `pin` (CLI:
    /// `wps_pin any <pin>`; D-Bus: `WPS.Start{Role:registrar,Pin}`).
    fn arm_wps_pin(&self, group_interface: &str, pin: &str) -> BackendResult<()>;

    /// Begin delivering [`P2pEvent`]s for the group interface on `tx`, blocking
    /// until the group is removed or the backend is told to stop. Called on the
    /// connection monitor thread. `running` is polled to allow cooperative
    /// shutdown.
    fn run_event_monitor(
        &self,
        group_interface: &str,
        tx: Sender<P2pEvent>,
        running: &std::sync::atomic::AtomicBool,
    );
}

pub mod cli;

#[cfg(feature = "dbus-backend")]
pub mod dbus;

/// Choose the backend at runtime. Defaults to the CLI (subprocess) backend;
/// selects the D-Bus backend only when compiled in AND explicitly requested via
/// `MIRACAST_BACKEND=dbus`.
///
/// `iface` is the interface the CLI backend should drive — the dedicated
/// supplicant's interface when one was started, or the configured P2P interface.
/// Without it the CLI backend falls back to `find_p2p_interface()`, which
/// auto-discovers the SYSTEM supplicant's `p2p-dev-*` and does not match the
/// dedicated supplicant's control socket (`ctrl_path`), so `wpa_cli` fails with
/// "Failed to connect to non-global ctrl_ifname". The D-Bus backend ignores it.
pub fn select_backend(
    ctrl_path: Option<String>,
    iface: Option<String>,
    go_candidates: Vec<crate::capabilities::GoCandidate>,
    won_resolution: std::sync::Arc<std::sync::Mutex<(u32, u32)>>,
) -> std::sync::Arc<dyn P2pBackend> {
    #[cfg(feature = "dbus-backend")]
    {
        let want_dbus = std::env::var("MIRACAST_BACKEND")
            .map(|v| v.eq_ignore_ascii_case("dbus"))
            .unwrap_or(false);
        if want_dbus {
            match dbus::DbusBackend::new() {
                Ok(b) => {
                    log::info!("P2P backend: wpa_supplicant D-Bus");
                    return std::sync::Arc::new(b);
                }
                Err(e) => {
                    log::warn!("D-Bus backend unavailable ({e}); falling back to wpa_cli");
                }
            }
        }
    }
    log::info!("P2P backend: wpa_cli (subprocess)");
    std::sync::Arc::new(
        cli::WpaCliBackend::new(ctrl_path)
            .with_interface(iface)
            .with_go_candidates(go_candidates, won_resolution),
    )
}
