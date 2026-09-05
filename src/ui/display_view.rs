//! Display view for the Ubuntu Miracast Server.
//!
//! Shows the current streaming state: idle (waiting + PIN), connected
//! (negotiating), or receiving (video display). Faithful port of
//! `src/miracast_server/ui/display_view.py`.
//!
//! The Python view reached back into the window/handler via `get_root()`.
//! In Rust we invert that: the app installs callback hooks
//! (`on_refresh_pin`, `on_disconnect`, `on_toggle_fullscreen`) that this view
//! invokes, so it needs no reference to the window or the connection handler.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;

use crate::events::StreamStats;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayState {
    Idle,
    Connected,
    Receiving,
}

impl DisplayState {
    fn name(self) -> &'static str {
        match self {
            DisplayState::Idle => "idle",
            DisplayState::Connected => "connected",
            DisplayState::Receiving => "receiving",
        }
    }
}

type Hook = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// Main display view showing stream status and video output.
pub struct DisplayView {
    root: gtk::Box,
    stack: gtk::Stack,
    device_name_label: gtk::Label,
    pin_box: gtk::Box,
    pin_label: gtk::Label,
    source_name_label: gtk::Label,
    video_picture: gtk::Picture,
    controls_revealer: gtk::Revealer,
    stats_revealer: gtk::Revealer,
    stats_label: gtk::Label,
    state: Rc<RefCell<DisplayState>>,
    controls_timeout: Rc<RefCell<Option<glib::SourceId>>>,

    on_refresh_pin: Hook,
    on_disconnect: Hook,
    on_toggle_fullscreen: Hook,
}

impl DisplayView {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let overlay = gtk::Overlay::builder().vexpand(true).hexpand(true).build();
        root.append(&overlay);

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        overlay.set_child(Some(&stack));

        // ── Idle view ──
        let idle_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        let idle_icon = gtk::Image::from_icon_name("video-display-symbolic");
        idle_icon.set_pixel_size(96);
        idle_icon.add_css_class("dim-label");
        idle_box.append(&idle_icon);
        let idle_label = gtk::Label::new(Some("Waiting for Miracast source..."));
        idle_label.add_css_class("title-2");
        idle_box.append(&idle_label);
        let device_name_label = gtk::Label::new(Some(""));
        device_name_label.add_css_class("dim-label");
        idle_box.append(&device_name_label);

        // Persistent PIN display.
        let pin_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(24)
            .halign(gtk::Align::Center)
            .build();
        let pin_header = gtk::Label::new(Some("Enter this PIN on your device:"));
        pin_header.add_css_class("dim-label");
        pin_box.append(&pin_header);
        let pin_label = gtk::Label::new(Some(""));
        pin_label.add_css_class("title-1");
        pin_label.set_selectable(true);
        pin_box.append(&pin_label);
        let refresh_pin_btn = gtk::Button::with_label("Refresh PIN");
        refresh_pin_btn.set_halign(gtk::Align::Center);
        refresh_pin_btn.set_margin_top(12);
        pin_box.append(&refresh_pin_btn);
        pin_box.set_visible(false);
        idle_box.append(&pin_box);

        stack.add_named(&idle_box, Some(DisplayState::Idle.name()));

        // ── Connected view ──
        let connected_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        let spinner = gtk::Spinner::new();
        spinner.set_spinning(true);
        spinner.set_size_request(48, 48);
        connected_box.append(&spinner);
        let connected_label = gtk::Label::new(Some("Source connected, waiting for stream..."));
        connected_label.add_css_class("title-3");
        connected_box.append(&connected_label);
        let source_name_label = gtk::Label::new(Some(""));
        source_name_label.add_css_class("dim-label");
        connected_box.append(&source_name_label);
        stack.add_named(&connected_box, Some(DisplayState::Connected.name()));

        // ── Receiving view ──
        let receiving_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .vexpand(true)
            .build();
        let video_picture = gtk::Picture::new();
        video_picture.set_vexpand(true);
        video_picture.set_hexpand(true);
        video_picture.set_can_shrink(true);
        receiving_box.append(&video_picture);
        stack.add_named(&receiving_box, Some(DisplayState::Receiving.name()));

        // ── Fullscreen overlay controls ──
        let controls_revealer = gtk::Revealer::builder()
            .valign(gtk::Align::End)
            .halign(gtk::Align::Center)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();
        overlay.add_overlay(&controls_revealer);
        let controls_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_bottom(24)
            .build();
        controls_box.add_css_class("toolbar");
        controls_box.add_css_class("osd");
        controls_revealer.set_child(Some(&controls_box));
        let disconnect_btn = gtk::Button::from_icon_name("media-playback-stop-symbolic");
        disconnect_btn.set_tooltip_text(Some("Disconnect"));
        controls_box.append(&disconnect_btn);
        let fullscreen_btn = gtk::Button::from_icon_name("view-fullscreen-symbolic");
        fullscreen_btn.set_tooltip_text(Some("Toggle Fullscreen"));
        controls_box.append(&fullscreen_btn);

        // ── Stats overlay (top-right) ──
        let stats_revealer = gtk::Revealer::builder()
            .valign(gtk::Align::Start)
            .halign(gtk::Align::End)
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .build();
        overlay.add_overlay(&stats_revealer);
        let stats_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_end(8)
            .build();
        stats_box.add_css_class("osd");
        stats_revealer.set_child(Some(&stats_box));
        let stats_label = gtk::Label::new(Some(""));
        stats_label.set_xalign(1.0);
        stats_label.add_css_class("caption");
        stats_box.append(&stats_label);

        let on_refresh_pin: Hook = Rc::new(RefCell::new(None));
        let on_disconnect: Hook = Rc::new(RefCell::new(None));
        let on_toggle_fullscreen: Hook = Rc::new(RefCell::new(None));

        let view = Self {
            root,
            stack,
            device_name_label,
            pin_box,
            pin_label,
            source_name_label,
            video_picture,
            controls_revealer,
            stats_revealer,
            stats_label,
            state: Rc::new(RefCell::new(DisplayState::Idle)),
            controls_timeout: Rc::new(RefCell::new(None)),
            on_refresh_pin,
            on_disconnect,
            on_toggle_fullscreen,
        };

        // Wire the buttons to the installable hooks.
        {
            let hook = view.on_refresh_pin.clone();
            refresh_pin_btn.connect_clicked(move |_| {
                if let Some(cb) = hook.borrow().as_ref() {
                    cb();
                }
            });
        }
        {
            let hook = view.on_disconnect.clone();
            disconnect_btn.connect_clicked(move |_| {
                if let Some(cb) = hook.borrow().as_ref() {
                    cb();
                }
            });
        }
        {
            let hook = view.on_toggle_fullscreen.clone();
            fullscreen_btn.connect_clicked(move |_| {
                if let Some(cb) = hook.borrow().as_ref() {
                    cb();
                }
            });
        }

        // Mouse-motion controller: reveal controls, auto-hide after 3s.
        {
            let motion = gtk::EventControllerMotion::new();
            let state = view.state.clone();
            let revealer = view.controls_revealer.clone();
            let timeout = view.controls_timeout.clone();
            motion.connect_motion(move |_, _, _| {
                if *state.borrow() != DisplayState::Receiving {
                    return;
                }
                revealer.set_reveal_child(true);
                if let Some(id) = timeout.borrow_mut().take() {
                    id.remove();
                }
                let revealer2 = revealer.clone();
                let timeout2 = timeout.clone();
                let id = glib::timeout_add_seconds_local(3, move || {
                    revealer2.set_reveal_child(false);
                    *timeout2.borrow_mut() = None;
                    glib::ControlFlow::Break
                });
                *timeout.borrow_mut() = Some(id);
            });
            view.root.add_controller(motion);
        }

        // Double-click on the video toggles fullscreen.
        {
            let gesture = gtk::GestureClick::new();
            gesture.set_button(1);
            let hook = view.on_toggle_fullscreen.clone();
            gesture.connect_released(move |_, n_press, _, _| {
                if n_press == 2 {
                    if let Some(cb) = hook.borrow().as_ref() {
                        cb();
                    }
                }
            });
            view.video_picture.add_controller(gesture);
        }

        view
    }

    /// Install the hook the "Refresh PIN" button calls (→ handler.rearm_wps_pin).
    pub fn set_on_refresh_pin<F: Fn() + 'static>(&self, f: F) {
        *self.on_refresh_pin.borrow_mut() = Some(Box::new(f));
    }
    /// Install the hook the disconnect button calls (→ handler.disconnect_peer).
    pub fn set_on_disconnect<F: Fn() + 'static>(&self, f: F) {
        *self.on_disconnect.borrow_mut() = Some(Box::new(f));
    }
    /// Install the hook the fullscreen button / double-click calls (→ window toggle).
    pub fn set_on_toggle_fullscreen<F: Fn() + 'static>(&self, f: F) {
        *self.on_toggle_fullscreen.borrow_mut() = Some(Box::new(f));
    }

    /// Transition to idle state.
    pub fn set_state_idle(&self, device_name: &str) {
        *self.state.borrow_mut() = DisplayState::Idle;
        self.device_name_label.set_text(
            if device_name.is_empty() {
                String::new()
            } else {
                format!("Discoverable as: {device_name}")
            }
            .as_str(),
        );
        self.stack.set_visible_child_name(DisplayState::Idle.name());
        self.stats_revealer.set_reveal_child(false);
        self.controls_revealer.set_reveal_child(false);
    }

    /// Display the WPS PIN persistently in the idle view.
    pub fn set_pin(&self, pin: &str) {
        self.pin_label.set_text(pin);
        self.pin_box.set_visible(true);
    }

    /// Hide the PIN display.
    pub fn hide_pin(&self) {
        self.pin_box.set_visible(false);
        self.pin_label.set_text("");
    }

    /// Transition to connected state.
    pub fn set_state_connected(&self, source_name: &str) {
        *self.state.borrow_mut() = DisplayState::Connected;
        self.source_name_label.set_text(source_name);
        self.stack
            .set_visible_child_name(DisplayState::Connected.name());
    }

    /// Transition to receiving state and bind the video paintable from the pipeline.
    pub fn set_state_receiving(&self, pipeline: Option<&gstreamer::Pipeline>) {
        *self.state.borrow_mut() = DisplayState::Receiving;
        self.stack
            .set_visible_child_name(DisplayState::Receiving.name());
        self.stats_revealer.set_reveal_child(true);
        if let Some(pipeline) = pipeline {
            self.attach_paintable(pipeline);
        }
    }

    /// Look up the gtk4paintablesink's paintable and set it on the Picture.
    pub fn attach_paintable(&self, pipeline: &gstreamer::Pipeline) {
        use gstreamer::prelude::*;
        if let Some(videosink) = pipeline.by_name("videosink") {
            // Guard: only gtk4paintablesink exposes a "paintable" property.
            if videosink.find_property("paintable").is_some() {
                let paintable = videosink.property::<gtk::gdk::Paintable>("paintable");
                self.video_picture.set_paintable(Some(&paintable));
            } else {
                log::warn!("videosink has no paintable property; skipping bind");
            }
        }
    }

    /// Update the stats overlay display (only while receiving).
    pub fn update_stats(&self, stats: &StreamStats) {
        if *self.state.borrow() != DisplayState::Receiving {
            return;
        }
        let (w, h) = stats.resolution;
        let res_str = if w > 0 {
            format!("{w}x{h}")
        } else {
            "Unknown".to_string()
        };
        let bitrate_mbps = if stats.bitrate != 0.0 {
            stats.bitrate / 1_000_000.0
        } else {
            0.0
        };
        let minutes = stats.duration / 60;
        let seconds = stats.duration % 60;
        self.stats_label.set_text(&format!(
            "{res_str} | {bitrate_mbps:.1} Mbps | {minutes}:{seconds:02}"
        ));
    }

    /// The root widget for embedding this view.
    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
}

impl Default for DisplayView {
    fn default() -> Self {
        Self::new()
    }
}
