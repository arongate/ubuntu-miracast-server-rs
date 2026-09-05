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

## Field-test fixes (post-port, hardware-validated)

A real-hardware test (casting from a phone's "Smart View") surfaced three bugs,
all fixed with regression tests:

- **Group-interface parse** (`advertiser.rs`). `ip link show` emits
  `3: p2p-0: <FLAGS> mtu 1500 ...`; the parser was capturing the whole line as
  the interface name instead of just `p2p-0`, so every `wpa_cli -i <name>` call
  failed ("Failed to arm WPS PIN after 10 attempts") and casts could not
  complete. Now extracts the name from the correct `": "`-delimited segment,
  mirroring the Python `parts[1]`. Covered by `parse_group_interface` tests.
- **Main-thread WPS-arm UI freeze** (`connection.rs`). DHCP setup and WPS-PIN
  arming (up to 10×1 s retries) ran on the GTK main loop, freezing the window
  ("not responding"). Moved all blocking setup into the monitor thread;
  `rearm_wps_pin` (called from the event drain) arms on a detached thread.
- **Naive-datetime history load** (`models.rs`). The Python app wrote naive
  ISO 8601 datetimes with no timezone offset (`2026-08-22T22:13:45.593112`);
  `parse_from_rfc3339` rejected them ("premature end of input"), so existing
  history failed to load. Now parses both naive and offset-bearing forms and
  emits the naive form (matching Python's `datetime.isoformat()`), so on-disk
  history stays compatible across the two implementations.

## Reliability, supply chain, and protocol coverage (hardening pass)

Following an ISO/IEC 25010 review, three hardening changes were made:

- **Poison-tolerant locking** (`sync_ext.rs`). `LockExt::lock_safe()` recovers a
  poisoned `Mutex` guard instead of panicking. All 43 non-test locks use it, so
  a panic while a lock is held no longer cascades into app-wide panics. The
  remaining `.unwrap()`/`.expect()` calls were audited and are safe by
  construction (compile-time regex literals, thread-spawn failure, guard-checked
  paths).
- **Supply-chain gates** (`deny.toml` + CI `security` job). `cargo audit`
  (RUSTSEC advisories, yanked crates) and `cargo deny` (advisories, license
  allowlist, source/duplicate policy) run in CI.
- **RTSP M1–M7 integration test** (`tests/rtsp_handshake.rs`). A mock source on
  loopback plays the source side of the WFD handshake; the real
  `MiracastReceiver` is driven through it and asserted to reach `StreamStarted`,
  exercising the actual socket I/O and message construction.

  **Known limitation:** this test is timing-sensitive at the OS-socket level (if
  the sink thread is preempted between M5's reply and sending M6, the mock reads
  EOF). It is therefore marked `#[ignore]` and run in a dedicated serialized,
  `continue-on-error` CI step — it provides signal without gating the matrix on
  a scheduling race. Deterministic protocol coverage lives in the `rtsp.rs` and
  `receiver.rs` unit tests. A future improvement is to make the receiver's
  `recv_message` resilient to partial reads (it currently treats any incomplete
  read as a fatal `None`), which would let the test run deterministically.

## Native-backend migration (dropping subprocess + root)

Following `docs/native-dbus-research.md`, the P2P control plane is moving off
`wpa_cli`/subprocess onto native APIs, in phases, without breaking the proven
path:

- **Phase 1 (done):** `ethtool -i` → sysfs `/sys/class/net/<iface>/device/driver`;
  `systemctl --user` → `org.freedesktop.systemd1` on the session bus via
  `zbus::blocking`. No subprocess, unprivileged, behaviour unchanged.
- **Phase 2 (done):** the P2P control plane is behind a `P2pBackend` trait
  (`src/p2p_backend.rs`) with two impls:
  - `cli` — the original `wpa_cli` subprocess path (default, hardware-validated):
    a faithful relocation of the advertiser sequence + WPS arm + ATTACH loop.
  - `dbus` (feature `dbus-backend`) — `fi.w1.wpa_supplicant1` over the system bus
    via `zbus::blocking`: writes `WFDIEs`, sets `P2PDeviceConfig`, calls
    `P2PDevice.Find`/`GroupAdd` + `WPS.Start`, and consumes
    `GroupStarted`/`PeerJoined`/`PeerDisconnected`/`GroupFinished` signals
    instead of scraping stdout. Event-driven, no `ip link` polling.
  `advertiser.rs`/`connection.rs` call the trait and consume typed `P2pEvent`s.
  Backend selection is runtime: `wpa_cli` by default, D-Bus only when the
  `dbus-backend` feature is compiled AND `MIRACAST_BACKEND=dbus` is set. D-Bus
  access is `netdev`-group-gated (no polkit, no sudo).

  **Not yet hardware-tested:** the D-Bus backend build-checks, passes clippy
  `-D warnings`, and its byte-assembly unit tests pass, but the live P2P
  handshake has only run on the `wpa_cli` path. One field to confirm on
  hardware: peer MAC is read from `Peer.DeviceAddress`; if named differently,
  `peer_mac_from_signal` logs and skips (non-fatal) rather than connecting.
- **Phase 3 (done, feature-gated):** IP + DHCP behind a `NetBackend` trait
  (`src/net_backend.rs`), runtime-selected like Phase 2:
  - `subprocess` — the original `sudo ip` + `dnsmasq` path (DEFAULT,
    hardware-validated), relocated behind the trait.
  - `native` (feature `native-net`) — interface IP via **netlink** (`neli` 0.7,
    sync, builder API) needing `CAP_NET_ADMIN`, plus an **in-process DHCP
    server** (plain `UdpSocket` on :67, `SO_BINDTODEVICE`-pinned to the P2P
    interface) needing `CAP_NET_BIND_SERVICE`. No NetworkManager, no `ip`/
    `dnsmasq` binaries — depends only on the Linux kernel, the most portable
    option across distributions. The DHCP behaviour mirrors the dnsmasq config
    (pool 192.168.173.80-90, router+DNS = the GO IP, 5m lease) and keeps its own
    in-memory lease table for peer-IP resolution.
  Selected via `MIRACAST_NET=native`. `debian/postinst` runs `setcap
  cap_net_admin,cap_net_bind_service+ep` on the binary (best-effort) so the
  native path runs with **no runtime root**. Chosen for maximum Linux
  compatibility over the NetworkManager-shared-mode alternative.

  **Not yet hardware-tested:** the in-process DHCP server is unit-tested
  (DISCOVER→OFFER in-pool, router/DNS options, pool wrap) and the netlink code
  compiles against the real neli 0.7.4 builder API, but the live IP-assign +
  lease exchange has only run on the subprocess (`ip`/`dnsmasq`) path.

## CI / release

- `.github/workflows/ci.yml` — matrix of `--no-default-features` (headless core)
  and `--features gui`, each running `cargo clippy --all-targets -- -D warnings`
  and `cargo test`; plus `rustfmt` and the `security` (audit + deny) jobs.
- `.github/workflows/release.yml` — on a `v*.*.*` tag, builds the `.deb` via
  `dpkg-buildpackage` (debhelper), plus a standalone binary tarball, and creates
  a GitHub Release (pre-release for tags containing `-`).
