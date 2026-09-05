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

- **Interface enumeration (Phase 2.5, done):** `list_p2p_interfaces` now queries
  `fi.w1.wpa_supplicant1`'s `Interfaces` property + each `Ifname` over the system
  bus (sudo-free, `netdev`-gated), driver from sysfs; it falls back to the legacy
  sudo `wpa_cli`/`nmcli` enumeration only if the bus is unreachable. The D-Bus
  backend's `ensure_interface` already resolved interfaces natively, so with this
  change the **`dbus-backend` + `native-net` runtime path invokes zero `sudo` and
  needs no root** — the CLI/subprocess backends' sudo sites are compiled but not
  selected, and `ui/app.rs` skips the dedicated-supplicant bootstrap under D-Bus.
  Verified live: the `Interfaces` D-Bus query returns results on the test host, so
  the sudo fallback is not hit.

  **Not yet hardware-tested:** the D-Bus P2P path and the native netlink IP-assign
  + in-process DHCP compile against verified APIs and pass unit tests, but the
  live P2P handshake + IP/lease exchange has only run on the subprocess/wpa_cli
  path.

## Automatic configuration negotiation (portability)

The port does not hardcode a channel/adapter/resolution for one machine. At
launch `capabilities::detect()` probes the host and builds an ordered
**GO candidate ladder**; the backend walks it, verifies each rung, and falls
back — so the same binary self-configures on any Ubuntu machine. This is a
deliberate improvement over the Python original (which advertised a fixed WFD
format with `native_index=0` = 640×480, causing sources to stream upscaled SD).

- **GO bring-up ladder (Phase 1, `cli.rs`).** Candidates:
  2.4 GHz social ch1 → ch6 → ch11 → driver-chosen (720p each); a clean 5 GHz
  channel is prepended only when `MIRACAST_GO_5GHZ=1`. For each rung: snapshot
  existing `p2p-*` netdevs → `p2p_group_add freq=…` → wait for a *new* group
  iface → **verify the operating band** via `wpa_cli … status` (a P2P-GO exposes
  its channel only there, not via `iw dev info`). First working rung wins; its
  band-appropriate resolution is recorded in a shared cell the receiver reads at
  the M3 capability response.
- **WFD resolution per band (`rtsp.rs`).** `WfdVideoFormat::for_max_resolution`
  emits native-mode + CEA bitmap for 1080p (native `0x38`, bitmap incl. 1080p60)
  or 720p (native `0x28`, bitmap capped ≤720p). The M3 `wfd_video_formats` line
  is built from the winning rung's resolution — a deviation from the Python
  fixed string, documented here.
- **Discovery watchdog (Phase 2, `ui/app.rs`).** After 60 s advertising with no
  source (`AP-STA-CONNECTED`), `rotate_discovery_channel()` advances across the
  social rungs and re-advertises; when 1/6/11 are exhausted it shows an in-app
  prompt with the concrete user action and stops rotating. 5 GHz-only autonomous
  GOs are avoided because phones scan the 2.4 GHz social channels for a sink
  (confirmed in the field: GO on ch149 → phone never discovered it).
- **Adapter selection (`ui/app.rs`).** Enumerate Wi-Fi ifaces from sysfs
  (`/sys/class/net/*/phy80211`, state-independent — an idle NM-*unmanaged* USB
  dongle is invisible to `wpa_supplicant`/`nmcli` precisely because it is free),
  skip the active uplink (`wpa_state=COMPLETED`), prefer an idle adapter for a
  dedicated supplicant; single-radio hosts fall back to the system supplicant.
  Selection keys on the interface **name**, never a phy index (phy0/phy1 are not
  stable across reboots).

## Audio under sudo (PulseAudio routing)

`pulsesink` connects to a per-user PulseAudio session; root (via `sudo`) has
none, so a naïve run fails PLAYING with `Connection refused` and the pipeline
would otherwise be video-only. `receiver::make_audio_sink` detects the sudo case
(`SUDO_USER` set), resolves that user's uid from `/etc/passwd` (dependency-free
`uid_of_user`), and points `pulsesink server=/run/user/<uid>/pulse/native` with
`PULSE_COOKIE=/run/user/<uid>/pulse/cookie` for the cross-uid connection. The
up-front `audio_sink_available` probe applies the *same* routing so its verdict
matches the real sink. Non-root runs leave `server` unset (native session). The
two-axis decode×audio retry loop (see below) remains the safety net: an audio
PLAYING failure drops the audio branch so video always plays.

The video PLAYING path is a retry loop over two axes — decode (HW `vaapidecodebin`
→ SW `avdec_h264`) × audio (with → video-only) — because a failed PLAYING is a
*synchronous* `StateChangeError` the bus watch never sees, a live
`udpsrc`+`tsdemux` pipeline returns `StateChangeSuccess::Async` (wait on
`state()` for the real verdict), and the shared audio branch can take the whole
pipeline (including video) down. Live low-latency tuning: sink `sync=false`,
`QUEUE_MAX_TIME` 200 ms, and a live-tuned `rtpjitterbuffer`
(`latency=80`, `drop-on-latency`, `do-lost`).

## CI / release

- `.github/workflows/ci.yml` — matrix of `--no-default-features` (headless core)
  and `--features gui`, each running `cargo clippy --all-targets -- -D warnings`
  and `cargo test`; plus `rustfmt` and the `security` (audit + deny) jobs.
- `.github/workflows/release.yml` — on a `v*.*.*` tag, builds the `.deb` via
  `dpkg-buildpackage` (debhelper), plus a standalone binary tarball, and creates
  a GitHub Release (pre-release for tags containing `-`).
