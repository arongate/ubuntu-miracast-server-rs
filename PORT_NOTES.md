# Rust Port — Fidelity Notes

This is a faithful Rust rewrite of [`arongate/ubuntu-miracast-server`](https://github.com/arongate/ubuntu-miracast-server)
(Python 3 / PyGObject, ~5,700 LOC). It preserves the original's **functional**
behaviour (Miracast sink: autonomous P2P Group Owner, WPS PIN arming, RTSP WFD
M1–M7 negotiation, GStreamer H.264/AAC decode, session history, headless service
mode, GTK4 GUI) and its **non-functional** behaviour (identical external-command
argv, byte-identical protocol strings and templates, same config/history file
formats and locations, same security validation).

## Build & run

```bash
# GUI (default)
cargo build --release
./target/release/ubuntu-miracast-server              # GTK GUI
./target/release/ubuntu-miracast-server --service    # headless

# Headless-only core (no GTK/GStreamer-gtk deps pulled for the GUI sink)
cargo build --release --no-default-features

# Tests (feature-independent unit tests: 46)
cargo test                       # gui build
cargo test --no-default-features # headless core
```

CLI flags mirror the Python argparse exactly: `--service`, `--fullscreen`,
`--interface <IFACE>`, `--name <NAME>`.

## Module map (Python → Rust)

| Python | Rust | Notes |
|---|---|---|
| `utils.py` | `src/utils.rs` | wpa_cli wrapper, injection allowlist, P2P iface detection |
| `rtsp.py` | `src/rtsp.rs` | RTSP parse/serialize + WFD params (byte-exact) |
| `models.py` | `src/models.rs` | validated structs + JSON (de)serialization |
| `config.py` | `src/config.rs` | JSON config, atomic 0600 write, validation rules |
| `history.py` | `src/history.rs` | session persistence, 500-cap, newest-first |
| `advertiser.py` | `src/advertiser.rs` | autonomous GO, WFD subelement hex |
| `connection.py` | `src/connection.rs` | WPS PIN arming, dnsmasq DHCP, AP-STA monitor |
| `p2p_supplicant.py` | `src/p2p_supplicant.rs` | dedicated wpa_supplicant lifecycle |
| `receiver.py` | `src/receiver.rs` | RTSP client M1–M7 + gstreamer-rs pipeline |
| `service.py` | `src/service.rs` | headless mode + systemd user-service manager |
| `app.py` | `src/ui/app.rs` | Adw.Application shell, event drain, signal wiring |
| `ui/main_window.py` | `src/ui/main_window.rs` | window, stack, nav, F11/Esc |
| `ui/display_view.py` | `src/ui/display_view.rs` | idle/connected/receiving states, paintable |
| `ui/settings_view.py` | `src/ui/settings_view.rs` | config UI bound to ServerConfig |
| `ui/sessions_view.py` | `src/ui/sessions_view.rs` | session history browser |

## Deliberate deviations (all fidelity-preserving)

- **Concurrency / signals.** PyGObject GObject signals + `GLib.idle_add` are
  replaced by an `mpsc` channel of typed `Event`s (`src/events.rs`) drained on
  the GTK main loop via `glib::timeout_add_local` (GUI) or `recv_timeout`
  (headless). The event variants map 1:1 to the Python `__gsignals__` entries,
  and the handler flow is identical.
- **View → app callbacks.** Python views reached the app via
  `get_root().get_application().<method>()`. In Rust the app installs closure
  hooks (`set_on_toggle_fullscreen`, `set_on_disconnect`, `set_on_refresh_pin`,
  `set_on_switch_interface`) that the views invoke — same effect, no upward
  reference.
- **GStreamer.** Bound in-process via `gstreamer-rs` (chosen for full fidelity):
  the dynamic pipeline, `tsdemux` pad-added linking, buffer pad-probe byte
  stats, bus-driven HW→SW decoder fallback, and `gtk4paintablesink` paintable
  binding all reproduce the Python behaviour. Pipeline element chain, caps, and
  queue bounds are identical.
- **External commands.** `wpa_cli`, `dnsmasq`, `ip`, `nmcli`, `systemctl`,
  `ethtool`, `wpa_supplicant`, `pkill` all use **identical argv**. The
  `wpa_supplicant` config template and the systemd unit template are
  **byte-identical** to the Python.
- **Protocol strings.** All WFD parameter strings, the M1–M7 RTSP messages, the
  capability body (lazycast values), and the WFD subelement hex
  (`000600111C44012C` for port 7236) are byte-for-byte preserved and covered by
  unit tests.
- **Subprocess timeouts.** Python `subprocess.run(timeout=)` has no std Rust
  equivalent, so a small spawn+poll helper enforces the same wall-clock budget
  and kills on expiry (callers treat any error as "command unavailable", exactly
  as the Python except-blocks did).
- **Signals.** SIGINT/SIGTERM in headless service mode are handled via a thin
  `signal()` binding flipping an `AtomicBool` the drain loop observes — same
  graceful-shutdown ordering (Receiver → ConnectionHandler → Advertiser →
  Supplicant).

Deviations that touch behaviour are marked with inline `// NOTE:` comments at
the site.

## API notes (crate versions)

- `gstreamer` 0.23: `Caps::from_string` → `Caps::from_str` (`std::str::FromStr`).
- `log` 0.4 needs the `std`/`alloc` feature for `set_boxed_logger`.
- GUI stack: `gtk4` 0.9, `libadwaita` 0.7, `gst-plugin-gtk4` 0.13
  (`gtk4paintablesink`, registered via `plugin_register_static()` at startup).
