//! Ubuntu Miracast Server — a Wi-Fi Display (Miracast) sink for Ubuntu.
//!
//! Faithful Rust port of the Python/PyGObject `miracast_server` package.
//! The headless core builds with `--no-default-features`; the GTK4 + libadwaita
//! GUI is gated behind the `gui` feature.

pub mod config;
pub mod events;
pub mod history;
pub mod models;
pub mod net_backend;
pub mod p2p_backend;
pub mod rtsp;
pub mod sync_ext;
pub mod utils;

pub mod advertiser;
pub mod connection;
pub mod p2p_supplicant;
pub mod receiver;
pub mod service;

#[cfg(feature = "gui")]
pub mod ui;
