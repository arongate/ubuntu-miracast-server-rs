//! Settings view for the Ubuntu Miracast Server.
//!
//! Grouped configuration options bound to `ServerConfig` with validation
//! feedback. Faithful port of `src/miracast_server/ui/settings_view.py`.
//!
//! The Python `_on_interface_changed` reached the app via
//! `get_root().get_application().switch_interface(...)`. Here the app installs
//! an `on_switch_interface` hook that this view calls instead.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use serde_json::json;

use crate::config::ServerConfig;
use crate::utils::list_p2p_interfaces;

type Config = Rc<RefCell<ServerConfig>>;
type InterfaceHook = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;

pub struct SettingsView {
    root: gtk::ScrolledWindow,
    config: Config,
    interface_row: adw::ComboRow,
    interface_list: gtk::StringList,
    interface_values: Rc<RefCell<Vec<String>>>,
    on_switch_interface: InterfaceHook,
}

impl SettingsView {
    pub fn new(config: Config) -> Self {
        let root = gtk::ScrolledWindow::new();
        root.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);

        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(16)
            .margin_end(16)
            .build();
        root.set_child(Some(&clamp));

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 24);
        clamp.set_child(Some(&vbox));

        let interface_list = gtk::StringList::new(&["(auto-detect)"]);
        let interface_row = adw::ComboRow::builder()
            .title("P2P Interface")
            .subtitle("Wi-Fi adapter to use for Miracast (empty = auto-detect)")
            .build();

        let view = Self {
            root,
            config,
            interface_row,
            interface_list,
            interface_values: Rc::new(RefCell::new(vec![String::new()])),
            on_switch_interface: Rc::new(RefCell::new(None)),
        };
        view.build(&vbox);
        view
    }

    /// Install the hook the interface combo calls when a new interface is chosen.
    pub fn set_on_switch_interface<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_switch_interface.borrow_mut() = Some(Box::new(f));
    }

    fn build(&self, vbox: &gtk::Box) {
        let cfg = &self.config;

        // ── General ──
        let general = adw::PreferencesGroup::builder().title("General").build();
        vbox.append(&general);

        let device_name_row = adw::EntryRow::builder().title("Device Name").build();
        device_name_row.set_text(&cfg.borrow().get_str(
            "general",
            "device_name",
            "Ubuntu Miracast Server",
        ));
        {
            let cfg = cfg.clone();
            device_name_row.connect_changed(move |row| {
                let value = row.text().trim().to_string();
                if !value.is_empty() {
                    let _ = cfg.borrow_mut().set("general", "device_name", json!(value));
                }
            });
        }
        general.add(&device_name_row);

        general.add(&self.switch_row(
            "Start Minimized",
            None,
            cfg.borrow().get_bool("general", "start_minimized", false),
            "general",
            "start_minimized",
        ));
        general.add(
            &self.switch_row(
                "Fullscreen on Stream",
                Some("Automatically enter fullscreen when stream starts"),
                cfg.borrow()
                    .get_bool("general", "fullscreen_on_stream", true),
                "general",
                "fullscreen_on_stream",
            ),
        );

        // Log level combo.
        let log_levels = ["DEBUG", "INFO", "WARNING", "ERROR"];
        let log_row = adw::ComboRow::builder().title("Log Level").build();
        log_row.set_model(Some(&gtk::StringList::new(&log_levels)));
        let current_level = cfg.borrow().get_str("general", "log_level", "INFO");
        log_row.set_selected(
            log_levels
                .iter()
                .position(|l| *l == current_level)
                .unwrap_or(1) as u32,
        );
        {
            let cfg = cfg.clone();
            log_row.connect_selected_notify(move |row| {
                let idx = row.selected() as usize;
                if let Some(level) = log_levels.get(idx) {
                    let _ = cfg.borrow_mut().set("general", "log_level", json!(level));
                }
            });
        }
        general.add(&log_row);

        // ── Streaming ──
        let streaming = adw::PreferencesGroup::builder().title("Streaming").build();
        vbox.append(&streaming);

        let rtsp_row = adw::EntryRow::builder().title("RTSP Port").build();
        rtsp_row.set_text(
            &cfg.borrow()
                .get_i64("streaming", "rtsp_port", 7236)
                .to_string(),
        );
        {
            let cfg = cfg.clone();
            rtsp_row.connect_changed(move |row| {
                let text = row.text().trim().to_string();
                match text.parse::<i64>() {
                    Ok(port) => {
                        // set() validates the 1024-65535 range; flag on rejection.
                        if cfg
                            .borrow_mut()
                            .set("streaming", "rtsp_port", json!(port))
                            .is_err()
                        {
                            row.add_css_class("error");
                        } else {
                            row.remove_css_class("error");
                        }
                    }
                    Err(_) => row.add_css_class("error"),
                }
            });
        }
        streaming.add(&rtsp_row);

        streaming.add(&self.switch_row(
            "Audio Enabled",
            None,
            cfg.borrow().get_bool("streaming", "audio_enabled", true),
            "streaming",
            "audio_enabled",
        ));

        let resolutions = ["1920x1080", "1280x720", "640x480"];
        let res_row = adw::ComboRow::builder().title("Max Resolution").build();
        res_row.set_model(Some(&gtk::StringList::new(&resolutions)));
        let current_res = cfg
            .borrow()
            .get_str("streaming", "max_resolution", "1920x1080");
        res_row.set_selected(
            resolutions
                .iter()
                .position(|r| *r == current_res)
                .unwrap_or(0) as u32,
        );
        {
            let cfg = cfg.clone();
            res_row.connect_selected_notify(move |row| {
                let idx = row.selected() as usize;
                if let Some(res) = resolutions.get(idx) {
                    let _ = cfg
                        .borrow_mut()
                        .set("streaming", "max_resolution", json!(res));
                }
            });
        }
        streaming.add(&res_row);

        let codec_row = adw::ComboRow::builder().title("Preferred Codec").build();
        codec_row.set_model(Some(&gtk::StringList::new(&["H264"])));
        codec_row.set_selected(0);
        streaming.add(&codec_row);

        // ── Network ──
        let network = adw::PreferencesGroup::builder().title("Network").build();
        vbox.append(&network);

        self.populate_interfaces();
        self.interface_row.set_model(Some(&self.interface_list));
        let current_iface = cfg.borrow().get_str("network", "p2p_interface", "");
        {
            let values = self.interface_values.borrow();
            let sel = values
                .iter()
                .position(|v| *v == current_iface && !current_iface.is_empty());
            self.interface_row.set_selected(sel.unwrap_or(0) as u32);
        }
        {
            let cfg = cfg.clone();
            let values = self.interface_values.clone();
            let hook = self.on_switch_interface.clone();
            self.interface_row.connect_selected_notify(move |row| {
                let idx = row.selected() as usize;
                let values = values.borrow();
                if let Some(new_iface) = values.get(idx) {
                    let _ = cfg
                        .borrow_mut()
                        .set("network", "p2p_interface", json!(new_iface));
                    if let Some(cb) = hook.borrow().as_ref() {
                        cb(new_iface.clone());
                    }
                }
            });
        }
        network.add(&self.interface_row);

        // Refresh interfaces row.
        let refresh_row = adw::ActionRow::builder()
            .title("Refresh Interfaces")
            .subtitle("Rescan for available P2P Wi-Fi adapters")
            .build();
        let refresh_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh_btn.set_valign(gtk::Align::Center);
        {
            let list = self.interface_list.clone();
            let values = self.interface_values.clone();
            let row = self.interface_row.clone();
            let cfg = cfg.clone();
            refresh_btn.connect_clicked(move |_| {
                // Reset to just "(auto-detect)".
                while list.n_items() > 1 {
                    list.remove(list.n_items() - 1);
                }
                *values.borrow_mut() = vec![String::new()];
                for info in list_p2p_interfaces() {
                    let label = if info.driver.is_empty() {
                        format!("{} ({})", info.interface, info.parent)
                    } else {
                        format!("{} ({} — {})", info.interface, info.parent, info.driver)
                    };
                    list.append(&label);
                    values.borrow_mut().push(info.interface);
                }
                let current = cfg.borrow().get_str("network", "p2p_interface", "");
                let values_b = values.borrow();
                let sel = values_b
                    .iter()
                    .position(|v| *v == current && !current.is_empty());
                row.set_selected(sel.unwrap_or(0) as u32);
            });
        }
        refresh_row.add_suffix(&refresh_btn);
        refresh_row.set_activatable_widget(Some(&refresh_btn));
        network.add(&refresh_row);

        network.add(&self.spin_row(
            "GO Intent",
            Some("Higher values prefer being Group Owner (0-15)"),
            0.0,
            15.0,
            1.0,
            cfg.borrow().get_i64("network", "go_intent", 15) as f64,
            "network",
            "go_intent",
        ));
        network.add(&self.spin_row(
            "Connection Timeout",
            Some("Seconds to wait for P2P group formation (1-120)"),
            1.0,
            120.0,
            1.0,
            cfg.borrow().get_i64("network", "connection_timeout", 30) as f64,
            "network",
            "connection_timeout",
        ));
        network.add(&self.switch_row(
            "Auto Accept Connections",
            Some("Automatically accept incoming Miracast connections"),
            cfg.borrow().get_bool("network", "auto_accept", true),
            "network",
            "auto_accept",
        ));

        // ── Service Mode ──
        let service = adw::PreferencesGroup::builder()
            .title("Service Mode")
            .build();
        vbox.append(&service);
        service.add(&self.switch_row(
            "Enable Service Mode",
            Some("Run as a background systemd user service"),
            cfg.borrow().get_bool("service", "enabled", false),
            "service",
            "enabled",
        ));
        service.add(&self.switch_row(
            "Virtual Display",
            Some("Use a virtual display in service mode"),
            cfg.borrow().get_bool("service", "virtual_display", false),
            "service",
            "virtual_display",
        ));
        service.add(&self.spin_row(
            "Idle Timeout",
            Some("Seconds before service exits when idle (0 = disabled)"),
            0.0,
            86400.0,
            60.0,
            cfg.borrow().get_i64("service", "idle_timeout", 0) as f64,
            "service",
            "idle_timeout",
        ));
    }

    fn switch_row(
        &self,
        title: &str,
        subtitle: Option<&str>,
        active: bool,
        section: &'static str,
        key: &'static str,
    ) -> adw::SwitchRow {
        let mut b = adw::SwitchRow::builder().title(title);
        if let Some(s) = subtitle {
            b = b.subtitle(s);
        }
        let row = b.build();
        row.set_active(active);
        let cfg = self.config.clone();
        row.connect_active_notify(move |r| {
            let _ = cfg.borrow_mut().set(section, key, json!(r.is_active()));
        });
        row
    }

    #[allow(clippy::too_many_arguments)]
    fn spin_row(
        &self,
        title: &str,
        subtitle: Option<&str>,
        min: f64,
        max: f64,
        step: f64,
        value: f64,
        section: &'static str,
        key: &'static str,
    ) -> adw::SpinRow {
        let row = adw::SpinRow::with_range(min, max, step);
        row.set_title(title);
        if let Some(s) = subtitle {
            row.set_subtitle(s);
        }
        row.set_value(value);
        let cfg = self.config.clone();
        row.connect_value_notify(move |r| {
            let _ = cfg.borrow_mut().set(section, key, json!(r.value() as i64));
        });
        row
    }

    fn populate_interfaces(&self) {
        for info in list_p2p_interfaces() {
            let label = if info.driver.is_empty() {
                format!("{} ({})", info.interface, info.parent)
            } else {
                format!("{} ({} — {})", info.interface, info.parent, info.driver)
            };
            self.interface_list.append(&label);
            self.interface_values.borrow_mut().push(info.interface);
        }
    }

    /// The root widget for embedding this view.
    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
}
