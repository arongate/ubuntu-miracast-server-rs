//! Main window for the Ubuntu Miracast Server.
//!
//! Adw.ApplicationWindow hosting a stack of Display / Sessions / Settings
//! pages, a header status label, a bottom nav bar, and window-level F11 /
//! Escape fullscreen handling. Faithful port of
//! `src/miracast_server/ui/main_window.py`.
//!
//! The Python window connected directly to component GObject signals. Here the
//! `ui::app` event drain calls the public `on_*` methods below instead, and the
//! views' action hooks are installed by the app — so this type owns only the
//! widgets and the view structs.

use std::rc::Rc;

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::gio;
use libadwaita as adw;
use adw::prelude::*;

use crate::ui::display_view::DisplayView;
use crate::ui::sessions_view::SessionsView;
use crate::ui::settings_view::SettingsView;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct MainWindow {
    window: adw::ApplicationWindow,
    status_label: gtk::Label,
    stack: gtk::Stack,
    display: Rc<DisplayView>,
    sessions: Rc<SessionsView>,
    settings: Rc<SettingsView>,
}

impl MainWindow {
    /// Build the window around already-constructed views.
    pub fn new(
        app: &adw::Application,
        display: Rc<DisplayView>,
        sessions: Rc<SessionsView>,
        settings: Rc<SettingsView>,
    ) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Ubuntu Miracast Server")
            .default_width(900)
            .default_height(600)
            .width_request(600)
            .height_request(400)
            .build();

        let main_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.set_content(Some(&main_box));

        // Header bar with status label + menu.
        let header = adw::HeaderBar::new();
        main_box.append(&header);
        let status_label = gtk::Label::new(Some("Initializing..."));
        status_label.add_css_class("dim-label");
        header.set_title_widget(Some(&status_label));

        let menu_button = gtk::MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        let menu_model = gio::Menu::new();
        menu_model.append(Some("Settings"), Some("win.show-settings"));
        menu_model.append(Some("About"), Some("win.show-about"));
        menu_button.set_menu_model(Some(&menu_model));
        header.pack_end(&menu_button);

        // Stack of pages.
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        main_box.append(&stack);
        stack.add_titled(&display.widget(), Some("display"), "Display");
        stack.add_titled(&sessions.widget(), Some("sessions"), "Sessions");
        stack.add_titled(&settings.widget(), Some("settings"), "Settings");

        // Bottom navigation bar.
        let bottom = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        bottom.add_css_class("toolbar");
        main_box.append(&bottom);
        for (page_id, icon, label) in [
            ("display", "video-display-symbolic", "Display"),
            ("sessions", "document-open-recent-symbolic", "Sessions"),
            ("settings", "preferences-system-symbolic", "Settings"),
        ] {
            let btn = gtk::Button::new();
            let btn_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .halign(gtk::Align::Center)
                .build();
            btn_box.append(&gtk::Image::from_icon_name(icon));
            btn_box.append(&gtk::Label::new(Some(label)));
            btn.set_child(Some(&btn_box));
            let stack2 = stack.clone();
            btn.connect_clicked(move |_| stack2.set_visible_child_name(page_id));
            bottom.append(&btn);
        }
        stack.set_visible_child_name("display");

        let this = Rc::new(Self {
            window: window.clone(),
            status_label,
            stack,
            display,
            sessions,
            settings,
        });

        this.register_actions(&window);
        this
    }

    fn register_actions(self: &Rc<Self>, window: &adw::ApplicationWindow) {
        // win.show-settings
        {
            let stack = self.stack.clone();
            let action = gio::SimpleAction::new("show-settings", None);
            action.connect_activate(move |_, _| stack.set_visible_child_name("settings"));
            window.add_action(&action);
        }
        // win.show-about
        {
            let win = window.clone();
            let action = gio::SimpleAction::new("show-about", None);
            action.connect_activate(move |_, _| {
                let about = adw::AboutWindow::builder()
                    .transient_for(&win)
                    .application_name("Ubuntu Miracast Server")
                    .application_icon("video-display")
                    .version(VERSION)
                    .developer_name("Ubuntu Miracast Project")
                    .comments("Receive Miracast wireless display streams")
                    .license_type(gtk::License::MitX11)
                    .build();
                about.present();
            });
            window.add_action(&action);
        }

        // Window-level F11 / Escape fullscreen handling (NFR-U03).
        let key_ctrl = gtk::EventControllerKey::new();
        let win = window.clone();
        key_ctrl.connect_key_pressed(move |_, keyval, _, _| {
            match keyval {
                gtk::gdk::Key::F11 => {
                    if win.is_fullscreen() {
                        win.unfullscreen();
                    } else {
                        win.fullscreen();
                    }
                    gtk::glib::Propagation::Stop
                }
                gtk::gdk::Key::Escape if win.is_fullscreen() => {
                    win.unfullscreen();
                    gtk::glib::Propagation::Stop
                }
                _ => gtk::glib::Propagation::Proceed,
            }
        });
        window.add_controller(key_ctrl);
    }

    pub fn present(&self) {
        self.window.present();
    }
    pub fn window(&self) -> adw::ApplicationWindow {
        self.window.clone()
    }
    pub fn fullscreen(&self) {
        self.window.fullscreen();
    }
    pub fn unfullscreen(&self) {
        self.window.unfullscreen();
    }
    pub fn is_fullscreen(&self) -> bool {
        self.window.is_fullscreen()
    }
    pub fn display(&self) -> &Rc<DisplayView> {
        &self.display
    }
    pub fn sessions(&self) -> &Rc<SessionsView> {
        &self.sessions
    }
    pub fn settings(&self) -> &Rc<SettingsView> {
        &self.settings
    }

    pub fn set_status(&self, text: &str) {
        self.status_label.set_text(text);
    }
}
