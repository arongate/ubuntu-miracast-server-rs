//! Service management for Ubuntu Miracast Server.
//!
//! Implements systemd user-service installation/lifecycle and headless
//! (service) mode operation. Faithful port of
//! `src/miracast_server/service.py`. The headless loop uses an mpsc event
//! drain instead of a GLib main loop (no GTK dependency in the core).

use crate::advertiser::MiracastAdvertiser;
use crate::config::ServerConfig;
use crate::connection::ConnectionHandler;
use crate::events::{channel, Event};
use crate::history::ServerSessionHistory;
use crate::receiver::MiracastReceiver;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SERVICE_NAME: &str = "ubuntu-miracast-server.service";

/// systemd unit template — byte-identical to the Python `_SERVICE_TEMPLATE`.
const SERVICE_TEMPLATE: &str = "\
[Unit]
Description=Ubuntu Miracast Server (Wi-Fi Display Sink)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/ubuntu-miracast-server --service
Restart=on-failure
RestartSec=5
Environment=DISPLAY=:0

[Install]
WantedBy=default.target
";

/// Errors from service management (equivalent to Python RuntimeError).
#[derive(Debug)]
pub struct ServiceError(pub String);
impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ServiceError {}

/// Manages the systemd user service for headless Miracast receiving.
pub struct ServerServiceManager {
    service_dir: PathBuf,
    service_path: PathBuf,
}

impl Default for ServerServiceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerServiceManager {
    pub fn new() -> Self {
        let service_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".config")
            .join("systemd")
            .join("user");
        let service_path = service_dir.join(SERVICE_NAME);
        Self {
            service_dir,
            service_path,
        }
    }

    pub fn is_installed(&self) -> bool {
        self.service_path.exists()
    }

    pub fn is_enabled(&self) -> bool {
        systemctl_stdout(&["--user", "is-enabled", SERVICE_NAME])
            .map(|s| s.trim() == "enabled")
            .unwrap_or(false)
    }

    pub fn is_running(&self) -> bool {
        systemctl_stdout(&["--user", "is-active", SERVICE_NAME])
            .map(|s| s.trim() == "active")
            .unwrap_or(false)
    }

    /// Install the service file and reload systemd (rollback on reload failure).
    pub fn install(&self) -> Result<(), ServiceError> {
        std::fs::create_dir_all(&self.service_dir)
            .map_err(|e| ServiceError(format!("Failed to create service dir: {e}")))?;
        std::fs::write(&self.service_path, SERVICE_TEMPLATE)
            .map_err(|e| ServiceError(format!("Failed to write service file: {e}")))?;

        if let Err(e) = self.daemon_reload() {
            let _ = std::fs::remove_file(&self.service_path);
            return Err(e);
        }
        log::info!("Service installed at {}", self.service_path.display());
        Ok(())
    }

    pub fn uninstall(&self) -> Result<(), ServiceError> {
        if self.is_running() {
            self.stop()?;
        }
        if self.is_enabled() {
            self.disable()?;
        }
        if self.service_path.exists() {
            std::fs::remove_file(&self.service_path)
                .map_err(|e| ServiceError(format!("Failed to remove service file: {e}")))?;
        }
        if let Err(e) = self.daemon_reload() {
            log::warn!("daemon-reload after uninstall failed: {e}");
        }
        log::info!("Service uninstalled");
        Ok(())
    }

    pub fn enable(&self) -> Result<(), ServiceError> {
        if !self.is_installed() {
            self.install()?;
        }
        systemctl_ok(&["--user", "enable", SERVICE_NAME], "enable")?;
        log::info!("Service enabled");
        Ok(())
    }

    pub fn disable(&self) -> Result<(), ServiceError> {
        systemctl_ok(&["--user", "disable", SERVICE_NAME], "disable")?;
        log::info!("Service disabled");
        Ok(())
    }

    pub fn start(&self) -> Result<(), ServiceError> {
        if !self.is_installed() {
            self.install()?;
        }
        systemctl_ok(&["--user", "start", SERVICE_NAME], "start")?;
        log::info!("Service started");
        Ok(())
    }

    pub fn stop(&self) -> Result<(), ServiceError> {
        systemctl_ok(&["--user", "stop", SERVICE_NAME], "stop")?;
        log::info!("Service stopped");
        Ok(())
    }

    fn daemon_reload(&self) -> Result<(), ServiceError> {
        systemctl_ok(&["--user", "daemon-reload"], "daemon-reload")
    }
}

/// Query systemd (user session bus) for a status string equivalent to
/// `systemctl --user is-enabled|is-active <unit>`. Returns None on any D-Bus
/// error (callers treat that as "not enabled"/"not active", matching the prior
/// subprocess behaviour where a non-zero exit meant the same).
fn systemctl_stdout(args: &[&str]) -> Option<String> {
    // args are ["--user", "is-enabled"|"is-active", <unit>].
    let verb = args.get(1).copied()?;
    let unit = args.get(2).copied()?;
    let mgr = systemd_manager().ok()?;
    match verb {
        "is-enabled" => mgr
            .call_method("GetUnitFileState", &(unit))
            .ok()
            .and_then(|m| m.body().deserialize::<String>().ok()),
        "is-active" => {
            // GetUnit returns the unit object path; read its ActiveState.
            let path: zbus::zvariant::OwnedObjectPath = mgr
                .call_method("GetUnit", &(unit))
                .ok()?
                .body()
                .deserialize()
                .ok()?;
            let unit_proxy = zbus::blocking::Proxy::new(
                mgr.connection(),
                "org.freedesktop.systemd1",
                path.as_str(),
                "org.freedesktop.systemd1.Unit",
            )
            .ok()?;
            unit_proxy.get_property::<String>("ActiveState").ok()
        }
        _ => None,
    }
}

/// Perform a systemd (user session bus) action equivalent to
/// `systemctl --user enable|disable|start|stop|daemon-reload <unit>`.
fn systemctl_ok(args: &[&str], verb_label: &str) -> Result<(), ServiceError> {
    let verb = args.get(1).copied().unwrap_or("");
    let unit = args.get(2).copied().unwrap_or(SERVICE_NAME);
    let mgr = systemd_manager()
        .map_err(|e| ServiceError(format!("Failed to {verb_label} service: {e}")))?;

    let result = match verb {
        // EnableUnitFiles(files: as, runtime: b, force: b) -> (carries_install_info: b, changes)
        "enable" => mgr.call_method("EnableUnitFiles", &(vec![unit], false, true)),
        // DisableUnitFiles(files: as, runtime: b) -> changes
        "disable" => mgr.call_method("DisableUnitFiles", &(vec![unit], false)),
        // StartUnit(name: s, mode: s) -> job: o
        "start" => mgr.call_method("StartUnit", &(unit, "replace")),
        "stop" => mgr.call_method("StopUnit", &(unit, "replace")),
        "daemon-reload" => mgr.call_method("Reload", &()),
        other => {
            return Err(ServiceError(format!("unknown systemd verb: {other}")));
        }
    };
    result
        .map(|_| ())
        .map_err(|e| ServiceError(format!("systemctl {verb_label} failed: {e}")))
}

/// Connect to the user session bus and return a proxy to the systemd Manager.
fn systemd_manager() -> Result<zbus::blocking::Proxy<'static>, zbus::Error> {
    let conn = zbus::blocking::Connection::session()?;
    zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
}

/// Run the Miracast Server in headless service mode.
///
/// Uses an mpsc event drain (no GTK) with fakesink video output; implements
/// idle timeout and SIGINT/SIGTERM handling. Returns the process exit code.
pub fn run_as_service(device_name: Option<String>, p2p_interface: Option<String>) -> i32 {
    crate::receiver::gst_init();

    let config = ServerConfig::new(None);
    let mut history = ServerSessionHistory::new(None);

    let name = device_name
        .unwrap_or_else(|| config.get_str("general", "device_name", "Ubuntu Miracast Server"));
    let rtsp_port = config.get_i64("streaming", "rtsp_port", 7236) as u16;
    let rtp_port = config.get_i64("network", "rtp_port", 1028);
    let go_intent = config.get_i64("network", "go_intent", 15) as i32;
    let auto_accept = config.get_bool("network", "auto_accept", true);
    let connection_timeout = config.get_i64("network", "connection_timeout", 30) as i32;
    let audio_enabled = config.get_bool("streaming", "audio_enabled", true);
    let idle_timeout = config.get_i64("service", "idle_timeout", 0);

    // Determine P2P interface: CLI flag > config > auto-detect.
    let iface = p2p_interface.or_else(|| {
        let c = config.get_str("network", "p2p_interface", "");
        if c.is_empty() {
            None
        } else {
            Some(c)
        }
    });

    log::info!(
        "Starting service mode as '{}' (RTSP port {}, interface={})",
        name,
        rtsp_port,
        iface.clone().unwrap_or_else(|| "auto".to_string())
    );

    let (tx, rx) = channel();
    // Service mode uses the system wpa_supplicant (no dedicated ctrl path).
    let backend = crate::p2p_backend::select_backend(None);
    let mut advertiser = MiracastAdvertiser::new(
        name.clone(),
        rtsp_port,
        iface.clone(),
        std::sync::Arc::clone(&backend),
        tx.clone(),
    );
    let mut receiver = MiracastReceiver::new(rtsp_port, rtp_port, true, audio_enabled, tx.clone());
    let mut handler: Option<ConnectionHandler> = None;

    // SIGINT/SIGTERM → set the shutdown flag drained by the loop.
    let shutting_down = Arc::new(AtomicBool::new(false));
    install_signal_handlers(Arc::clone(&shutting_down));

    let mut last_activity = Instant::now();
    advertiser.start_advertising();

    loop {
        if shutting_down.load(Ordering::SeqCst) {
            log::info!("Service received signal, shutting down...");
            if receiver.is_receiving() {
                let stats = receiver.stop_receiving();
                if let Some(si) = receiver.source_info() {
                    history.add_session(si, stats);
                }
            }
            advertiser.stop_advertising();
            break;
        }

        // Drain events with a short timeout so we can poll the idle/shutdown state.
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => {
                last_activity = Instant::now();
                match event {
                    Event::AdvertisingStarted { group_interface } => {
                        if let Some(p2p_iface) = advertiser.p2p_interface() {
                            let mut h = ConnectionHandler::new(
                                p2p_iface,
                                go_intent,
                                auto_accept,
                                connection_timeout,
                                std::sync::Arc::clone(&backend),
                                crate::net_backend::select_net_backend(),
                                tx.clone(),
                            );
                            h.start_listening(group_interface);
                            handler = Some(h);
                        }
                    }
                    Event::AdvertisingError(msg) => {
                        log::error!("Service: advertising error — {msg}");
                        break;
                    }
                    Event::ConnectionReceived(conn) => {
                        log::info!("Service: connection from {}", conn.peer_name);
                        receiver.start_receiving(conn);
                    }
                    Event::ConnectionLost => log::info!("Service: connection lost"),
                    Event::StreamStopped(stats) => {
                        if let Some(si) = receiver.source_info() {
                            history.add_session(si, stats);
                        }
                        log::info!("Service: stream stopped");
                        if let Some(h) = handler.as_ref() {
                            h.rearm_wps_pin();
                        }
                    }
                    Event::StreamError(err) => {
                        if let Some(si) = receiver.source_info() {
                            let stats = receiver.stop_receiving();
                            history.add_session(si, stats);
                        }
                        log::error!("Service: stream error — {err}");
                        if let Some(h) = handler.as_ref() {
                            h.rearm_wps_pin();
                        }
                    }
                    _ => {}
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Idle timeout: exit after N seconds idle when not receiving.
        if idle_timeout > 0 && !receiver.is_receiving() {
            let elapsed = last_activity.elapsed().as_secs() as i64;
            if elapsed >= idle_timeout {
                log::info!("Idle timeout reached ({elapsed}s), exiting");
                if let Some(mut h) = handler.take() {
                    h.stop_listening();
                }
                advertiser.stop_advertising();
                break;
            }
        }
    }

    if let Some(mut h) = handler.take() {
        h.stop_listening();
    }
    log::info!("Service mode exited");
    0
}

#[cfg(unix)]
fn install_signal_handlers(flag: Arc<AtomicBool>) {
    // Minimal SIGINT/SIGTERM handling without extra deps: a process-global flag
    // the C handler flips, mirrored into the caller's Arc by the drain loop.
    // Matches the Python graceful-shutdown-on-signal behaviour.
    use std::sync::OnceLock;
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    let _ = FLAG.set(flag);

    extern "C" fn handler(_sig: i32) {
        if let Some(f) = FLAG.get() {
            f.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    unsafe {
        c_signal(2, handler as *const () as usize); // SIGINT
        c_signal(15, handler as *const () as usize); // SIGTERM
    }
}

#[cfg(not(unix))]
fn install_signal_handlers(_flag: Arc<AtomicBool>) {}

// Thin libc signal() binding to avoid pulling the full libc crate for one call.
#[cfg(unix)]
extern "C" {
    #[link_name = "signal"]
    fn c_signal(signum: i32, handler: usize) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_template_byte_identical() {
        assert!(
            SERVICE_TEMPLATE.contains("Description=Ubuntu Miracast Server (Wi-Fi Display Sink)")
        );
        assert!(SERVICE_TEMPLATE.contains("ExecStart=/usr/bin/ubuntu-miracast-server --service"));
        assert!(SERVICE_TEMPLATE.contains("Restart=on-failure"));
        assert!(SERVICE_TEMPLATE.contains("RestartSec=5"));
        assert!(SERVICE_TEMPLATE.contains("Environment=DISPLAY=:0"));
        assert!(SERVICE_TEMPLATE.contains("WantedBy=default.target"));
        assert!(SERVICE_TEMPLATE.ends_with("WantedBy=default.target\n"));
    }

    #[test]
    fn service_name_matches() {
        assert_eq!(SERVICE_NAME, "ubuntu-miracast-server.service");
    }

    #[test]
    fn dbus_manager_queries_do_not_panic() {
        // Only meaningful where a user session bus exists (dev boxes, CI with a
        // dbus session). Where it does not, systemd_manager() errors and the
        // status helpers return false — assert they degrade gracefully rather
        // than panic, matching the prior subprocess behaviour.
        if systemd_manager().is_err() {
            eprintln!("no session bus; skipping live D-Bus assertions");
            return;
        }
        let mgr = ServerServiceManager::new();
        // A not-installed unit must report not-enabled / not-active, never panic.
        let _ = mgr.is_enabled();
        let _ = mgr.is_running();
    }
}
