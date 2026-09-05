//! Sessions history view for the Ubuntu Miracast Server.
//!
//! Faithful port of `src/miracast_server/ui/sessions_view.py`. Displays a list
//! of past streaming sessions with details (source name, date, duration,
//! resolution, data received) and a clear-history action with confirmation.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::history::ServerSessionHistory;
use crate::models::ServerSessionRecord;

/// View displaying past streaming session history.
///
/// Shows session records with source name, date, duration, resolution, and
/// data received. Provides a clear-history button with confirmation.
///
/// The Python class subclassed `Gtk.Box`; here we hold the root `gtk::Box`
/// and expose it via [`SessionsView::widget`].
pub struct SessionsView {
    root: gtk::Box,
    history: Rc<RefCell<ServerSessionHistory>>,
    list_box: gtk::ListBox,
    clear_btn: gtk::Button,
}

impl SessionsView {
    /// Initialize the sessions view.
    ///
    /// `history` is a shared `ServerSessionHistory`.
    pub fn new(history: Rc<RefCell<ServerSessionHistory>>) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .build();

        let view = Self {
            root,
            history,
            list_box: gtk::ListBox::new(),
            clear_btn: gtk::Button::new(),
        };
        view.setup_ui();
        view.refresh();
        view
    }

    /// Set up the UI layout.
    fn setup_ui(&self) {
        // Toolbar with title and clear button.
        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_start(16)
            .margin_end(16)
            .margin_top(12)
            .margin_bottom(8)
            .build();
        self.root.append(&toolbar);

        let title = gtk::Label::builder()
            .label("Session History")
            .hexpand(true)
            .xalign(0.0)
            .build();
        title.add_css_class("title-3");
        toolbar.append(&title);

        self.clear_btn.set_label("Clear History");
        self.clear_btn.add_css_class("destructive-action");
        {
            let history = self.history.clone();
            let list_box = self.list_box.clone();
            let clear_btn = self.clear_btn.clone();
            self.clear_btn.connect_clicked(move |button| {
                Self::on_clear_clicked(button, &history, &list_box, &clear_btn);
            });
        }
        toolbar.append(&self.clear_btn);

        // Scrollable list.
        let scrolled = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .build();
        self.root.append(&scrolled);

        self.list_box.set_selection_mode(gtk::SelectionMode::None);
        self.list_box.add_css_class("boxed-list");
        self.list_box.set_margin_start(16);
        self.list_box.set_margin_end(16);
        self.list_box.set_margin_bottom(16);
        scrolled.set_child(Some(&self.list_box));

        // Placeholder for empty state.
        let placeholder = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .valign(gtk::Align::Center)
            .build();
        let placeholder_icon = gtk::Image::from_icon_name("document-open-recent-symbolic");
        placeholder_icon.set_pixel_size(48);
        placeholder_icon.add_css_class("dim-label");
        placeholder.append(&placeholder_icon);
        let placeholder_label = gtk::Label::new(Some("No sessions yet"));
        placeholder_label.add_css_class("dim-label");
        placeholder.append(&placeholder_label);
        self.list_box.set_placeholder(Some(&placeholder));
    }

    /// Refresh the session list from history.
    pub fn refresh(&self) {
        Self::refresh_list(&self.history, &self.list_box, &self.clear_btn);
    }

    /// Shared refresh implementation usable from both the public method and the
    /// clear callback (which only captures the widgets it needs).
    fn refresh_list(
        history: &Rc<RefCell<ServerSessionHistory>>,
        list_box: &gtk::ListBox,
        clear_btn: &gtk::Button,
    ) {
        // Clear existing rows.
        while let Some(row) = list_box.row_at_index(0) {
            list_box.remove(&row);
        }

        // Add session rows.
        let sessions = history.borrow().get_sessions();
        for record in &sessions {
            let row = Self::create_session_row(record);
            list_box.append(&row);
        }

        // Update clear button sensitivity.
        clear_btn.set_sensitive(!sessions.is_empty());
    }

    /// Create a list row for a session record.
    fn create_session_row(record: &ServerSessionRecord) -> adw::ActionRow {
        let row = adw::ActionRow::new();

        // Title: source name.
        let name = &record.source_info.name;
        let title = if name.is_empty() {
            "Unknown Source"
        } else {
            name.as_str()
        };
        row.set_title(title);

        // Subtitle: date + duration + resolution + data.
        let timestamp_str = record.timestamp.format("%Y-%m-%d %H:%M").to_string();

        let duration_min = record.stats.duration / 60;
        let duration_sec = record.stats.duration % 60;
        let duration_str = if duration_min != 0 {
            format!("{duration_min}m {duration_sec}s")
        } else {
            format!("{duration_sec}s")
        };

        let (res_w, res_h) = record.stats.resolution;
        let res_str = if res_w > 0 {
            format!("{res_w}x{res_h}")
        } else {
            String::new()
        };

        let data_mb = record.stats.data_received as f64 / (1024.0 * 1024.0);
        let data_str = format!("{data_mb:.1} MB");

        let mut parts: Vec<String> = vec![timestamp_str, duration_str];
        if !res_str.is_empty() {
            parts.push(res_str);
        }
        parts.push(data_str);

        row.set_subtitle(&parts.join(" · "));

        // Icon.
        let icon = gtk::Image::from_icon_name("video-display-symbolic");
        row.add_prefix(&icon);

        row
    }

    /// Handle clear-history button click with a confirmation dialog.
    fn on_clear_clicked(
        button: &gtk::Button,
        history: &Rc<RefCell<ServerSessionHistory>>,
        list_box: &gtk::ListBox,
        clear_btn: &gtk::Button,
    ) {
        let dialog = adw::MessageDialog::new(
            button.root().and_downcast::<gtk::Window>().as_ref(),
            Some("Clear Session History?"),
            Some(
                "This will permanently remove all session records. \
                 This action cannot be undone.",
            ),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("clear", "Clear");
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));

        let history = history.clone();
        let list_box = list_box.clone();
        let clear_btn = clear_btn.clone();
        dialog.connect_response(None, move |_dialog, response| {
            if response == "clear" {
                history.borrow_mut().clear();
                Self::refresh_list(&history, &list_box, &clear_btn);
                log::info!("Session history cleared by user");
            }
        });
        dialog.present();
    }

    /// The root widget for embedding this view.
    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
}
