//! Signal scaffold replacing PyGObject GObject signals + `GLib.idle_add`.
//!
//! Each component takes an `EventSender` and pushes typed events; the owning
//! loop (GUI or headless service) drains them on its main thread. This mirrors
//! the Python signal wiring 1:1 — the event variants are named after the
//! original `__gsignals__` entries.

use crate::models::{IncomingConnection, ReceiverStats};
use std::sync::mpsc::{Receiver, Sender};

/// Events emitted by the advertiser, connection handler, and receiver.
///
/// Variant names map to the Python signals:
///   advertising-started/-stopped/-error, connection-received/-lost/-error,
///   pin-display, stream-started/-stopped/-error, stats-updated,
///   resolution-changed.
#[derive(Debug, Clone)]
pub enum Event {
    // ── MiracastAdvertiser ──
    AdvertisingStarted {
        group_interface: String,
    },
    AdvertisingStopped,
    AdvertisingError(String),

    // ── ConnectionHandler ──
    ConnectionReceived(IncomingConnection),
    ConnectionLost,
    ConnectionError(String),
    /// (pin, peer_info)
    PinDisplay {
        pin: String,
        peer_info: String,
    },

    // ── MiracastReceiver ──
    StreamStarted,
    StreamStopped(ReceiverStats),
    StreamError(String),
    StatsUpdated(StreamStats),
    ResolutionChanged {
        width: u32,
        height: u32,
    },
}

/// Payload for `stats-updated` (Python emitted a dict; we use a struct).
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    pub bitrate: f64,
    pub peak_bitrate: f64,
    pub frames_decoded: i64,
    pub frames_dropped: i64,
    pub resolution: (u32, u32),
    pub data_received: i64,
    pub duration: i64,
}

/// Cloneable sender handed to components (thread-safe).
pub type EventSender = Sender<Event>;
/// Drained by the owning loop.
pub type EventReceiver = Receiver<Event>;

/// Create a fresh event channel.
pub fn channel() -> (EventSender, EventReceiver) {
    std::sync::mpsc::channel()
}
