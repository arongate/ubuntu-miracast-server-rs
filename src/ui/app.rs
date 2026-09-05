//! Main application shell for the Ubuntu Miracast Server GUI.
//!
//! Faithful port of `src/miracast_server/app.py`. Bootstraps the
//! Adw.Application, instantiates the core components, wires them together, and
//! drains the component event channel on the GTK main loop (replacing the
//! PyGObject signals + `GLib.idle_add` pattern).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::advertiser::MiracastAdvertiser;
use crate::config::ServerConfig;
use crate::connection::ConnectionHandler;
use crate::events::{channel, Event, EventReceiver};
use crate::history::ServerSessionHistory;
use crate::p2p_supplicant::P2PSupplicantManager;
use crate::receiver::MiracastReceiver;
use crate::ui::main_window::MainWindow;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

/// Shared, single-threaded application state (GTK is single-threaded; all of
/// this lives on the main loop, so `Rc<RefCell<..>>` is the right tool).
struct App {
    config: Rc<RefCell<ServerConfig>>,
    history: Rc<RefCell<ServerSessionHistory>>,
    advertiser: Rc<RefCell<MiracastAdvertiser>>,
    connection_handler: Rc<RefCell<Option<ConnectionHandler>>>,
    receiver: Rc<RefCell<MiracastReceiver>>,
    p2p_supplicant: Rc<RefCell<Option<P2PSupplicantManager>>>,
    window: Rc<MainWindow>,
    event_sender: crate::events::EventSender,
    device_name: String,
    go_intent: i32,
    auto_accept: bool,
    connection_timeout: i32,
    shutting_down: Rc<RefCell<bool>>,
}

/// Entry point invoked by `main.rs` for the GUI build.
pub fn run_gui(
    device_name: Option<String>,
    start_fullscreen: bool,
    p2p_interface: Option<String>,
) -> i32 {
    crate::receiver::gst_init();
    // Register the gtk4paintablesink plugin so "gtk4paintablesink" resolves.
    let _ = gstgtk4::plugin_register_static();

    let application = adw::Application::builder()
        .application_id("com.ubuntu.miracast-server")
        .build();

    let dn = device_name.clone();
    let iface = p2p_interface.clone();
    application.connect_activate(move |app| {
        activate(app, dn.clone(), start_fullscreen, iface.clone());
    });

    // Do not pass argv to GTK (matches app.run(sys.argv[:1])).
    let empty: [&str; 0] = [];
    application.run_with_args(&empty).value()
}

fn activate(
    app: &adw::Application,
    device_name_override: Option<String>,
    start_fullscreen: bool,
    p2p_interface_override: Option<String>,
) {
    let config = Rc::new(RefCell::new(ServerConfig::new(None)));
    let history = Rc::new(RefCell::new(ServerSessionHistory::new(None)));

    let device_name = device_name_override.unwrap_or_else(|| {
        config
            .borrow()
            .get_str("general", "device_name", "Ubuntu Miracast Server")
    });
    let rtsp_port = config.borrow().get_i64("streaming", "rtsp_port", 7236) as u16;
    let rtp_port = config.borrow().get_i64("network", "rtp_port", 1028);
    let go_intent = config.borrow().get_i64("network", "go_intent", 15) as i32;
    let auto_accept = config.borrow().get_bool("network", "auto_accept", true);
    let connection_timeout = config.borrow().get_i64("network", "connection_timeout", 30) as i32;
    let audio_enabled = config.borrow().get_bool("streaming", "audio_enabled", true);

    // CLI flag > config > auto-detect.
    let p2p_interface = p2p_interface_override.or_else(|| {
        let c = config.borrow().get_str("network", "p2p_interface", "");
        if c.is_empty() {
            None
        } else {
            Some(c)
        }
    });

    // Try a dedicated wpa_supplicant on a secondary adapter so the primary
    // adapter can stay on Wi-Fi (internet) while the secondary handles P2P.
    //
    // The D-Bus backend talks to the SYSTEM wpa_supplicant (netdev-group-gated,
    // no sudo), so a sudo-spawned dedicated instance is both redundant and would
    // break the root-free guarantee — skip it when D-Bus is selected.
    let dbus_selected = std::env::var("MIRACAST_BACKEND")
        .map(|v| v.eq_ignore_ascii_case("dbus"))
        .unwrap_or(false)
        && cfg!(feature = "dbus-backend");
    let p2p_supplicant = if dbus_selected {
        log::info!("D-Bus backend selected — using system wpa_supplicant, no dedicated instance");
        None
    } else {
        start_dedicated_supplicant(&p2p_interface, &device_name)
    };
    let (effective_interface, ctrl_path) = match &p2p_supplicant {
        Some(s) => (
            Some(s.interface().to_string()),
            Some(s.ctrl_path().to_string()),
        ),
        None => (None, None),
    };

    let (tx, rx) = channel();

    // One P2P backend (wpa_cli by default; wpa_supplicant D-Bus when selected),
    // shared by the advertiser and the connection handler. Seed it with the
    // dedicated supplicant's interface (falling back to the configured one) so
    // it drives that instance's control socket, not the system supplicant's.
    let backend = crate::p2p_backend::select_backend(
        ctrl_path.clone(),
        effective_interface
            .clone()
            .or_else(|| p2p_interface.clone()),
    );

    let advertiser = Rc::new(RefCell::new(MiracastAdvertiser::new(
        device_name.clone(),
        rtsp_port,
        effective_interface
            .clone()
            .or_else(|| p2p_interface.clone()),
        Arc::clone(&backend),
        tx.clone(),
    )));
    let receiver = Rc::new(RefCell::new(MiracastReceiver::new(
        rtsp_port,
        rtp_port,
        false,
        audio_enabled,
        tx.clone(),
    )));

    // Build views + window.
    let display = Rc::new(crate::ui::display_view::DisplayView::new());
    let sessions = Rc::new(crate::ui::sessions_view::SessionsView::new(history.clone()));
    let settings = Rc::new(crate::ui::settings_view::SettingsView::new(config.clone()));
    let window = MainWindow::new(app, display.clone(), sessions.clone(), settings.clone());

    let connection_handler: Rc<RefCell<Option<ConnectionHandler>>> = Rc::new(RefCell::new(None));

    let state = Rc::new(App {
        config: config.clone(),
        history: history.clone(),
        advertiser: advertiser.clone(),
        connection_handler: connection_handler.clone(),
        receiver: receiver.clone(),
        p2p_supplicant: Rc::new(RefCell::new(p2p_supplicant)),
        window: window.clone(),
        event_sender: tx.clone(),
        device_name: device_name.clone(),
        go_intent,
        auto_accept,
        connection_timeout,
        shutting_down: Rc::new(RefCell::new(false)),
    });

    install_view_hooks(&state);

    // Present + initial state.
    window.display().set_state_idle(&device_name);
    window.present();
    if start_fullscreen {
        window.fullscreen();
    }

    // Drain component events on the main loop.
    start_event_drain(state.clone(), rx);

    // Start advertising AFTER the window has had a chance to paint. The GO
    // setup does a bounded-but-blocking wait for the group interface; running
    // it synchronously here would delay the first paint (the user saw "no UI").
    // Defer it to the next main-loop iteration so the window shows first.
    {
        let advertiser = advertiser.clone();
        glib::idle_add_local_once(move || {
            advertiser.borrow_mut().start_advertising();
        });
    }

    // Graceful shutdown on window close / application shutdown.
    {
        let state = state.clone();
        app.connect_shutdown(move |_| graceful_shutdown(&state));
    }

    log::info!("Application activated");
}

/// Poll the component event channel on the GTK main loop and dispatch each
/// event to the same handler flow the Python signal callbacks implemented.
fn start_event_drain(state: Rc<App>, rx: EventReceiver) {
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        // Drain everything currently queued this tick.
        while let Ok(event) = rx.try_recv() {
            handle_event(&state, event);
        }
        glib::ControlFlow::Continue
    });
}

fn handle_event(state: &Rc<App>, event: Event) {
    let win = &state.window;
    match event {
        Event::AdvertisingStarted { group_interface } => {
            win.set_status(&format!("Advertising as '{}'", state.device_name));
            win.display().set_state_idle(&state.device_name);
            on_advertiser_started(state, &group_interface);
        }
        Event::AdvertisingStopped => win.set_status("Advertising stopped"),
        Event::AdvertisingError(msg) => {
            win.set_status(&format!("Error: {msg}"));
            log::error!("Advertising error: {msg}");
        }
        Event::ConnectionReceived(conn) => {
            win.set_status(&format!("Connected: {}", conn.peer_name));
            win.display().hide_pin();
            win.display().set_state_connected(&conn.peer_name);
            if state.receiver.borrow().is_receiving() {
                log::warn!("Ignoring new connection — already receiving");
            } else {
                log::info!(
                    "Source connected ({}) — starting RTSP client to {}:7236",
                    conn.peer_address,
                    conn.peer_ip
                );
                state.receiver.borrow_mut().start_receiving(conn);
            }
        }
        Event::ConnectionLost => {
            win.set_status("Connection lost");
            win.display().set_state_idle(&state.device_name);
        }
        Event::ConnectionError(msg) => {
            win.set_status(&format!("Advertising as '{}'", state.device_name));
            win.display().set_state_idle(&state.device_name);
            log::error!("Connection error: {msg}");
            return_to_advertising(state);
        }
        Event::PinDisplay { pin, peer_info } => {
            win.set_status(&format!("PIN: {pin} — Waiting for source to connect"));
            win.display().set_pin(&pin);
            log::info!("Displaying PIN {pin} for peer {peer_info}");
        }
        Event::StreamStarted => {
            win.set_status("Receiving stream");
            let pipeline = state.receiver.borrow().pipeline();
            win.display().set_state_receiving(pipeline.as_ref());
            if state
                .config
                .borrow()
                .get_bool("general", "fullscreen_on_stream", true)
            {
                win.fullscreen();
            }
        }
        Event::StreamStopped(stats) => {
            win.set_status("Stream ended");
            if let Some(si) = state.receiver.borrow().source_info() {
                state.history.borrow_mut().add_session(si, stats);
            }
            state.window.sessions().refresh();
            win.display().set_state_idle(&state.device_name);
            if win.is_fullscreen() {
                win.unfullscreen();
            }
            return_to_advertising(state);
        }
        Event::StreamError(msg) => {
            win.set_status("Stream error");
            // Take source_info in its own scope so the immutable borrow is
            // released before stop_receiving() takes a mutable borrow — the
            // `if let Some(_) = borrow()` guard would otherwise hold the shared
            // borrow across borrow_mut() and panic "RefCell already borrowed".
            let source_info = state.receiver.borrow().source_info();
            if let Some(si) = source_info {
                let stats = state.receiver.borrow_mut().stop_receiving();
                state.history.borrow_mut().add_session(si, stats);
                state.window.sessions().refresh();
            }
            win.display().set_state_idle(&state.device_name);
            if win.is_fullscreen() {
                win.unfullscreen();
            }
            log::error!("Stream error: {msg}");
            return_to_advertising(state);
        }
        Event::StatsUpdated(stats) => win.display().update_stats(&stats),
        Event::ResolutionChanged { .. } => {}
    }
}

/// When the GO is created, arm WPS + start listening on the group interface.
fn on_advertiser_started(state: &Rc<App>, group_interface: &str) {
    if group_interface.is_empty() {
        return;
    }
    log::info!("P2P GO active on {group_interface} — starting connection handler");

    let mut handler = ConnectionHandler::new(
        state
            .advertiser
            .borrow()
            .p2p_interface()
            .unwrap_or_default(),
        state.go_intent,
        state.auto_accept,
        state.connection_timeout,
        state.advertiser.borrow().backend(),
        crate::net_backend::select_net_backend(),
        state.event_sender.clone(),
    );
    handler.start_listening(group_interface);
    *state.connection_handler.borrow_mut() = Some(handler);
}

/// Re-arm the WPS PIN for the next connection (Autonomous GO stays active).
fn return_to_advertising(state: &Rc<App>) {
    if *state.shutting_down.borrow() {
        return;
    }
    if let Some(h) = state.connection_handler.borrow().as_ref() {
        h.rearm_wps_pin();
        log::info!("Ready for next connection (WPS PIN re-armed)");
    }
}

/// Switch the P2P interface at runtime (called by the settings view hook).
fn switch_interface(state: &Rc<App>, new_interface: String) {
    if new_interface.is_empty() {
        return;
    }
    log::info!("Switching P2P interface to: {new_interface}");

    if state.advertiser.borrow().is_advertising() {
        state.advertiser.borrow_mut().stop_advertising();
    }
    if let Some(h) = state.connection_handler.borrow_mut().as_mut() {
        if h.is_listening() {
            h.stop_listening();
        }
    }
    *state.connection_handler.borrow_mut() = None;

    state
        .advertiser
        .borrow_mut()
        .set_p2p_interface(&new_interface);
    let _ =
        state
            .config
            .borrow_mut()
            .set("network", "p2p_interface", serde_json::json!(new_interface));

    state.advertiser.borrow_mut().start_advertising();
    log::info!("Interface switched to {new_interface} — restarting advertising");
}

fn install_view_hooks(state: &Rc<App>) {
    let win = &state.window;

    // Fullscreen toggle from the display view's button / double-click.
    {
        let window = win.window();
        win.display().set_on_toggle_fullscreen(move || {
            if window.is_fullscreen() {
                window.unfullscreen();
            } else {
                window.fullscreen();
            }
        });
    }
    // Disconnect button → connection_handler.disconnect_peer().
    {
        let handler = state.connection_handler.clone();
        win.display().set_on_disconnect(move || {
            if let Some(h) = handler.borrow().as_ref() {
                h.disconnect_peer();
            }
        });
    }
    // Refresh-PIN button → connection_handler.rearm_wps_pin().
    {
        let handler = state.connection_handler.clone();
        win.display().set_on_refresh_pin(move || {
            if let Some(h) = handler.borrow().as_ref() {
                h.rearm_wps_pin();
            }
        });
    }
    // Interface switch from settings.
    {
        let state = state.clone();
        win.settings().set_on_switch_interface(move |iface| {
            switch_interface(&state, iface);
        });
    }
}

/// Try to start a dedicated wpa_supplicant on a secondary adapter.
fn start_dedicated_supplicant(
    p2p_interface: &Option<String>,
    device_name: &str,
) -> Option<P2PSupplicantManager> {
    use crate::utils::list_wifi_interfaces_sysfs;

    // Find a suitable adapter: P2P-capable AND not carrying the internet
    // connection. An explicit config/CLI interface wins outright.
    let target_iface: Option<String> = if let Some(p2p) = p2p_interface {
        Some(
            p2p.strip_prefix("p2p-dev-")
                .map(|s| s.to_string())
                .unwrap_or_else(|| p2p.clone()),
        )
    } else {
        // Auto-select from SYSFS, not from wpa_supplicant's D-Bus/nmcli lists.
        // The ideal dedicated adapter is an idle USB dongle that NM is NOT
        // managing — which is precisely why it is ABSENT from those lists
        // (unmanaged → not adopted by the system supplicant, shown "unmanaged"
        // by nmcli). sysfs lists every 802.11 netdev regardless of up/down or
        // managed state, so we can see it and then bring it up. Among the
        // candidates, skip whichever is the active Wi-Fi uplink
        // (wpa_state=COMPLETED) and prefer an idle one.
        let candidates = list_wifi_interfaces_sysfs();
        log::debug!("Wi-Fi adapters (sysfs): {}", candidates.join(", "));
        let pick = candidates
            .iter()
            .find(|iface| !adapter_on_wifi(iface))
            .cloned();
        if let Some(ref p) = pick {
            log::info!("Auto-selected {p} for dedicated P2P supplicant");
        } else if !candidates.is_empty() {
            log::info!(
                "All Wi-Fi adapters ({}) are carrying a connection; \
                 using system wpa_supplicant",
                candidates.join(", ")
            );
        }
        pick
    };

    let target_iface = match target_iface {
        Some(t) => t,
        None => {
            log::info!("No dedicated adapter available; using system wpa_supplicant");
            return None;
        }
    };

    // Guard the explicit-interface path too (auto-select already filtered).
    if adapter_on_wifi(&target_iface) {
        log::info!(
            "Adapter {target_iface} is connected to Wi-Fi — not starting dedicated supplicant"
        );
        return None;
    }

    let mut mgr = P2PSupplicantManager::new(target_iface.clone(), device_name);
    match mgr.start() {
        Ok(()) => {
            log::info!("Dedicated P2P wpa_supplicant started on {target_iface}");
            Some(mgr)
        }
        Err(e) => {
            log::warn!("Could not start dedicated supplicant: {e} — falling back");
            None
        }
    }
}

/// True if `iface` is currently carrying an associated Wi-Fi (STA) connection,
/// i.e. it is the internet uplink and must NOT be repurposed as a dedicated P2P
/// GO. Probes the system wpa_supplicant for `wpa_state=COMPLETED`. Best-effort:
/// a probe that cannot run (no supplicant control on this iface) reads as "not
/// on Wi-Fi", which is the safe default for an idle adapter.
fn adapter_on_wifi(iface: &str) -> bool {
    use std::process::Command;
    Command::new("sudo")
        .args(["wpa_cli", "-i", iface, "status"])
        .output()
        .map(|out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout).contains("wpa_state=COMPLETED")
        })
        .unwrap_or(false)
}

/// Orderly shutdown: Receiver → ConnectionHandler → Advertiser → Supplicant.
fn graceful_shutdown(state: &Rc<App>) {
    if *state.shutting_down.borrow() {
        return;
    }
    *state.shutting_down.borrow_mut() = true;
    log::info!("Initiating graceful shutdown...");

    if state.receiver.borrow().is_receiving() {
        let stats = state.receiver.borrow_mut().stop_receiving();
        if let Some(si) = state.receiver.borrow().source_info() {
            state.history.borrow_mut().add_session(si, stats);
        }
    }
    if let Some(mut h) = state.connection_handler.borrow_mut().take() {
        h.disconnect_peer();
        h.stop_listening();
    }
    state.advertiser.borrow_mut().stop_advertising();
    if let Some(mut s) = state.p2p_supplicant.borrow_mut().take() {
        s.stop();
    }
    log::info!("Graceful shutdown complete");
}
