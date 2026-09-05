# Ubuntu Miracast Server

[![CI](https://github.com/arongate/ubuntu-miracast-server/actions/workflows/ci.yml/badge.svg)](https://github.com/arongate/ubuntu-miracast-server/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/arongate/ubuntu-miracast-server?include_prereleases)](https://github.com/arongate/ubuntu-miracast-server/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Python 3.10+](https://img.shields.io/badge/python-3.10+-blue.svg)](https://www.python.org/downloads/)

A Wi-Fi Display (Miracast) sink for Ubuntu — receive wireless screen casts from any Miracast source device.

## ⚠️ Unstable Phase (0.x)

This project is in its initial development phase (`0.x.y`). Per [SemVer §4](https://semver.org/#spec-item-4), the public API is not yet stable — any release may introduce breaking changes. Pin your dependency to an exact version if you rely on this package.

## Features

- **Receive Miracast streams** — accept screen casts from phones, tablets, laptops, and other Miracast sources
- **Automatic discovery** — advertises as a WFD sink via Wi-Fi Direct P2P, discoverable by any Miracast source
- **Hardware-accelerated decoding** — uses VA-API or NVDEC when available, falls back to software (avdec_h264)
- **Audio support** — decodes AAC audio alongside H.264 video
- **Fullscreen display** — auto-fullscreen on stream start, toggle with F11/double-click, exit with Escape
- **Session history** — tracks past streaming sessions with stats (duration, resolution, bitrate, data)
- **Headless service mode** — run as a systemd user service for always-on reception without a GUI
- **Modern UI** — GTK 4 + libadwaita interface following GNOME HIG
- **Configurable** — device name, ports, auto-accept, resolution preferences, and more

## Screenshots

<!-- TODO: Add screenshots -->
*Screenshots coming soon.*

## Quick Start

```bash
# Install build + runtime dependencies
sudo apt install \
    cargo rustc pkg-config \
    libgtk-4-dev libadwaita-1-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    wpasupplicant dnsmasq

# Clone and build
git clone https://github.com/arongate/ubuntu-miracast-server.git
cd ubuntu-miracast-server
cargo build --release

# Run
./target/release/ubuntu-miracast-server
```

Your machine will appear as "Ubuntu Miracast Server" in the Miracast/wireless display list on source devices. A PIN will be displayed on screen — enter it on your phone/laptop to connect and start casting.

## Prerequisites

- **Ubuntu 24.04 LTS** (or compatible Linux distribution with GTK 4, GStreamer 1.20+)
- **Rust 1.75+** (via `rustup` or the distro `cargo`/`rustc` packages)
- **Wi-Fi adapter with P2P (Wi-Fi Direct) support**
- **wpa_supplicant** running with P2P enabled

> **Note:** Not all Wi-Fi adapters support P2P mode. Intel Wi-Fi 6 (AX200/AX201) and Qualcomm Atheros adapters are known to work. Check with `iw phy | grep P2P`.

> **Important: Single-radio limitation.** Most laptop Wi-Fi adapters cannot simultaneously maintain a regular Wi-Fi connection (to your router) and a P2P connection (for Miracast). If you have a **secondary USB Wi-Fi adapter** (e.g., TP-Link AXE5400), the app will automatically use it for P2P while your built-in adapter stays connected to the internet — no configuration needed. Without a second adapter, you may need to disconnect from Wi-Fi to cast. See [Troubleshooting](docs/getting-started.md#single-radio-wi-fi-limitation-concurrent-connection-failure) for details.

### System Dependencies

```bash
sudo apt install \
    cargo rustc pkg-config \
    libgtk-4-dev libadwaita-1-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    wpasupplicant dnsmasq

# Optional: hardware-accelerated video decoding
sudo apt install gstreamer1.0-vaapi
```

## Installation

### From Source (recommended for development)

```bash
git clone https://github.com/arongate/ubuntu-miracast-server.git
cd ubuntu-miracast-server

# Build the GUI (default) or the headless-only core
cargo build --release
cargo build --release --no-default-features   # headless, no GTK sink

# Install the binary
sudo install -Dm755 target/release/ubuntu-miracast-server /usr/local/bin/ubuntu-miracast-server
```

### From Debian Package

Download the `.deb` from the [latest release](https://github.com/arongate/ubuntu-miracast-server/releases):

```bash
sudo apt install ./ubuntu-miracast-server_*.deb
```

## Usage

### GUI Mode (default)

```bash
ubuntu-miracast-server
```

The application will:
1. Start advertising as a Miracast sink on your Wi-Fi Direct interface
2. Display a WPS PIN — enter it on your source device to connect
3. Once connected, negotiate the stream and display the received video

### How it configures itself (automatic, per-machine)

You do not hand-tune the radio for your hardware — the app detects the machine's
capabilities at launch and self-negotiates the best working configuration,
falling back and retrying until something works:

- **Adapter selection.** Wi-Fi adapters are enumerated from sysfs
  (`/sys/class/net/*/phy80211`), which sees adapters regardless of up/down or
  NetworkManager state. If a *second, idle* adapter exists (e.g. a USB dongle),
  the app starts a dedicated `wpa_supplicant` on it so your primary adapter
  stays on Wi-Fi. On a single-radio laptop it falls back to the system
  supplicant on the connected interface (Intel iwlwifi and similar can run
  STA + P2P-GO concurrently).
- **GO channel ladder (bring-up).** The Group Owner is brought up by trying an
  ordered ladder — **2.4 GHz social channel 1 → 6 → 11 → driver-chosen** — and
  *verifying* each rung actually came up on the expected band (via
  `wpa_cli … status`). The first rung that works is used. 2.4 GHz social
  channels are chosen because that is where phones scan for a Miracast sink.
  (A clean 5 GHz channel can be forced with `MIRACAST_GO_5GHZ=1` for more
  bandwidth, but a 5 GHz-only autonomous GO is often not discoverable — opt-in.)
- **Resolution per band.** The advertised max resolution follows the band the GO
  came up on: 720p on 2.4 GHz (fits the link, smooth), 1080p on 5 GHz.
- **Discovery rotation.** If no source connects within 60 s, the app rotates to
  the next social channel and re-advertises automatically. Only after all of
  1/6/11 have been tried does it show an in-app prompt telling you what to do
  (open Smart View / Cast, toggle the phone's Wi-Fi and rescan).
- **Video pipeline.** Hardware H.264 decode (VA-API) is tried first with an
  automatic fall-back to software (`avdec_h264`); live low-latency tuning
  (sink `sync=false`, small queues, a tuned RTP jitter buffer) keeps latency
  down. If the audio sink cannot open, the pipeline degrades to video-only
  rather than failing.

### CLI Options

```
ubuntu-miracast-server [OPTIONS]

Options:
  --service           Run in headless service mode (no GUI, uses fakesink)
  --fullscreen        Start the window in fullscreen mode
  --name NAME         Override the advertised device name
  --interface IFACE   Override the P2P Wi-Fi interface (auto-detected if omitted)
  --help              Show help message
```

### Service Mode

Run as a background service without a GUI:

```bash
ubuntu-miracast-server --service --name "Living Room Display"
```

Or install as a systemd user service:

```bash
# The application can manage its own service file
# See docs/service-mode.md for details
systemctl --user enable ubuntu-miracast-server
systemctl --user start ubuntu-miracast-server
```

## Configuration

Configuration is stored at `~/.config/ubuntu-miracast-server/config.json` and is created automatically on first run.

Key options:

| Section | Key | Default | Description |
|---------|-----|---------|-------------|
| general | device_name | "Ubuntu Miracast Server" | Advertised device name |
| general | fullscreen_on_stream | true | Auto-fullscreen when stream starts |
| streaming | rtsp_port | 7236 | RTSP port on source (standard WFD port) |
| network | rtp_port | 1028 | Local UDP port for RTP media reception |
| network | go_intent | 15 | P2P Group Owner intent (0-15) |
| network | auto_accept | true | Auto-accept incoming connections |
| network | p2p_interface | "" | P2P interface override (auto-detected if empty) |
| service | idle_timeout | 0 | Exit service after N seconds idle (0=disabled) |

See [docs/configuration.md](docs/configuration.md) for the full reference.

## Permissions & audio (running with vs without root)

The **default build** drives `wpa_supplicant`/`wpa_cli`, a dedicated
supplicant, `ip`, and `dnsmasq`, which need root — so it is normally launched
with `sudo`:

```bash
sudo ubuntu-miracast-server
```

### Audio under sudo

`pulsesink` connects to a per-*user* PulseAudio session, and root has none — so a
naïve run under `sudo` plays **video but no audio** (`pulsesink: Connection
refused`). The app handles this automatically: when launched via `sudo`
(`SUDO_USER` is set) it routes the audio sink to the *invoking user's*
PulseAudio — it resolves that user's uid from `/etc/passwd` and points
`pulsesink server=/run/user/<uid>/pulse/native` (plus `PULSE_COOKIE` for the
cross-uid connection). No configuration needed; you get sound under `sudo`.
If the cross-uid connection is still refused (some locked-down setups), the
pipeline degrades to video-only and logs the reason — run non-root for reliable
audio.

### Running without sudo (root-free)

To run as your normal user (no `sudo`, native audio), grant the binary the two
capabilities it needs and put your user in the `netdev` group so it can reach
the `wpa_supplicant` control socket:

```bash
# Capabilities (netlink IP config + binding the DHCP port). NOTE: setcap is
# cleared by every rebuild — re-apply after each `cargo build`.
sudo setcap 'cap_net_admin,cap_net_bind_service+ep' ./target/release/ubuntu-miracast-server

# One-time: join netdev (log out/in for it to take effect)
sudo usermod -aG netdev "$USER"

# Then run as yourself — PulseAudio works natively, no sudo:
./target/release/ubuntu-miracast-server
```

The Debian package's `postinst` applies the `setcap` grant on install. The
experimental fully root-free path (`--features dbus-backend,native-net`, env
`MIRACAST_BACKEND=dbus MIRACAST_NET=native`) replaces `wpa_cli`/`dnsmasq` with
the `wpa_supplicant` D-Bus API + in-process DHCP; see PORT_NOTES for its
hardware limitations.

## Documentation

- [Getting Started](docs/getting-started.md) — detailed setup and first-run guide
- [Architecture](docs/architecture.md) — module design, signal flow, threading model
- [Configuration](docs/configuration.md) — all options with descriptions and validation rules
- [Service Mode](docs/service-mode.md) — headless operation and systemd integration

## Project Structure

```
ubuntu-miracast-server/
├── src/miracast_server/
│   ├── app.py              # Application entry point, lifecycle, signal wiring
│   ├── advertiser.py       # P2P Group Owner creation and WFD advertisement
│   ├── connection.py       # WPS PIN arming, DHCP, AP-STA-CONNECTED monitoring
│   ├── p2p_supplicant.py   # Dedicated wpa_supplicant instance manager
│   ├── rtsp.py             # RTSP protocol parsing and WFD message building
│   ├── receiver.py         # RTSP client (connects to source) + GStreamer pipeline
│   ├── config.py           # Configuration management with validation
│   ├── history.py          # Session history persistence
│   ├── models.py           # Data models with validation
│   ├── service.py          # Systemd service manager and headless mode
│   ├── utils.py            # Security-validated wpa_cli helpers
│   └── ui/
│       ├── main_window.py  # Main application window
│       ├── display_view.py # Video display with fullscreen support
│       ├── sessions_view.py# Session history browser
│       └── settings_view.py# Configuration UI
├── tests/                  # pytest test suite (250 tests)
├── scripts/                # Security audit scripts
├── .github/workflows/      # CI/CD (lint, test, security, release)
├── docs/                   # User documentation
├── specs/                  # Design specifications
└── .kiro/                  # AI agent configuration + protocol knowledge
```

> **Note:** the tree above is the original Python layout. This Rust port mirrors
> those modules under `src/*.rs` and `src/ui/*.rs` — see
> [`PORT_NOTES.md`](PORT_NOTES.md) for the full module map and the list of
> fidelity-preserving deviations. The Rust source tree is:
>
> ```
> ubuntu-miracast-server/
> ├── Cargo.toml            # gui (default) / --no-default-features headless core
> ├── src/
> │   ├── main.rs           # CLI + logging + GUI/service dispatch
> │   ├── lib.rs            # module wiring
> │   ├── events.rs         # typed event channel (replaces GObject signals)
> │   ├── utils.rs advertiser.rs connection.rs p2p_supplicant.rs
> │   ├── rtsp.rs receiver.rs models.rs config.rs history.rs service.rs
> │   └── ui/               # app.rs main_window.rs display_view.rs
> │                         # settings_view.rs sessions_view.rs (feature = "gui")
> ├── debian/               # Cargo-based .deb packaging
> └── docs/                 # User documentation (verbatim)
> ```

## Development

```bash
# Build (GUI default) / headless core
cargo build
cargo build --no-default-features

# Run tests
cargo test
cargo test --no-default-features

# Lint + format
cargo clippy --all-targets
cargo fmt
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full development guide.

## CI/CD

Every push and pull request runs automated checks:

- **Lint** — `cargo clippy` + `cargo fmt --check`
- **Test** — `cargo test` on both the GUI and `--no-default-features` builds
- **Commit Lint** — conventional commits enforcement

Releases are automated via tag push (`v*.*.*`):
- Changelog generated from conventional commits (git-cliff)
- Debian `.deb` package built and attached
- GitHub Release created (pre-release for `-beta`/`-rc` tags)

## Related Projects

- [ubuntu-miracast-client](https://github.com/arongate/ubuntu-miracast-client) — the companion Miracast source (sender) application

## License

This project is licensed under the MIT License — see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- [lazycast](https://github.com/homeworkc/lazycast) for the proven Autonomous GO + WPS PIN approach
- [wpa_supplicant](https://w1.fi/wpa_supplicant/) for Wi-Fi Direct P2P support
- [GStreamer](https://gstreamer.freedesktop.org/) for media pipeline infrastructure
- [GTK](https://gtk.org/) and [libadwaita](https://gnome.pages.gitlab.gnome.org/libadwaita/) for the UI framework
- The Wi-Fi Display (Miracast) specification by the Wi-Fi Alliance
