//! GTK4 + libadwaita GUI for the Ubuntu Miracast Server (feature = "gui").

pub mod app;
pub mod display_view;
pub mod main_window;
pub mod sessions_view;
pub mod settings_view;

pub use app::run_gui;
