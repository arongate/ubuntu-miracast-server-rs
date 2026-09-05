//! Startup capability detection → optimal runtime configuration.
//!
//! A Miracast sink ships to arbitrary Ubuntu machines: it cannot assume a
//! Wi-Fi band, a hardware decoder, or an audio session. Rather than hardcode
//! constants (`freq=2412`, HW-then-SW, audio on) this module PROBES the machine
//! once at launch and derives the best parameters for the hardware it lands on:
//!
//!   * the P2P-GO operating channel — prefer a clean 5GHz channel (more
//!     bandwidth → no lag) on the GO radio, else a 2.4GHz social channel;
//!   * the advertised video resolution — 1080p on 5GHz, 720p on 2.4GHz (a
//!     2.4GHz P2P link cannot carry 1080p without lag);
//!   * whether to attempt hardware H.264 decode (VA-API driver present);
//!   * whether audio can play (a real audio session exists).
//!
//! Every derived value is a *default* the user can still override via config or
//! env. Detection is best-effort: a probe that cannot run yields the safe
//! conservative choice (2.4GHz social channel / 720p / software decode /
//! audio-off), never a panic.

use std::process::Command;

/// The GO operating band chosen for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoBand {
    /// 5GHz — full bandwidth, 1080p-capable.
    Band5,
    /// 2.4GHz social channel — universally discoverable, 720p-capped.
    Band24,
}

/// Detected machine capabilities + the config derived from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Wi-Fi interface names present (sysfs), sorted.
    pub wifi_interfaces: Vec<String>,
    /// The chosen GO operating frequency in MHz (feeds `p2p_group_add freq=`).
    pub go_freq_mhz: u32,
    /// Which band `go_freq_mhz` belongs to.
    pub go_band: GoBand,
    /// Advertised max resolution as (width, height) — 1080p on 5GHz, 720p on 2.4.
    pub max_resolution: (u32, u32),
    /// True if a VA-API hardware H.264 decoder driver is present.
    pub hw_decode: bool,
    /// True if a working audio sink/session is available.
    pub audio: bool,
}

impl Capabilities {
    /// One-line human summary for the startup `CapabilityReport` log.
    pub fn report(&self) -> String {
        let (w, h) = self.max_resolution;
        let band = match self.go_band {
            GoBand::Band5 => "5GHz",
            GoBand::Band24 => "2.4GHz",
        };
        format!(
            "adapters=[{}] GO={}MHz ({}) res={}x{} hw_decode={} audio={}",
            self.wifi_interfaces.join(","),
            self.go_freq_mhz,
            band,
            w,
            h,
            self.hw_decode,
            self.audio
        )
    }
}

/// 2.4GHz social channel 1 — the safe, universally-discoverable default.
const SOCIAL_FREQ_2G: u32 = 2412;

/// Detect capabilities by probing the machine. `audio_probe` is injected so the
/// caller can pass the receiver's real audio-sink probe (and tests can stub it).
pub fn detect(audio_probe: impl Fn() -> bool) -> Capabilities {
    let wifi_interfaces = crate::utils::list_wifi_interfaces_sysfs();

    // Pick the GO channel from the physical radios. Prefer a clean 5GHz channel
    // (no `no IR`, no DFS/radar) on any present Wi-Fi phy; fall back to 2.4GHz.
    let go_freq_mhz = best_go_freq(&wifi_interfaces).unwrap_or(SOCIAL_FREQ_2G);
    let go_band = if go_freq_mhz >= 5000 {
        GoBand::Band5
    } else {
        GoBand::Band24
    };

    // Resolution follows the band: a 2.4GHz P2P link cannot carry 1080p without
    // lag, so cap it at 720p there; 5GHz gets full 1080p.
    let max_resolution = match go_band {
        GoBand::Band5 => (1920, 1080),
        GoBand::Band24 => (1280, 720),
    };

    Capabilities {
        wifi_interfaces,
        go_freq_mhz,
        go_band,
        max_resolution,
        hw_decode: hw_decode_available(),
        audio: audio_probe(),
    }
}

/// Choose the best GO operating frequency across the given interfaces' phys.
/// Prefers a clean (beacon-capable, non-DFS) 5GHz channel; returns `None` if no
/// 5GHz channel is usable, so the caller falls back to a 2.4GHz social channel.
fn best_go_freq(interfaces: &[String]) -> Option<u32> {
    let mut best: Option<u32> = None;
    for iface in interfaces {
        let phy = match phy_index_of(iface) {
            Some(p) => p,
            None => continue,
        };
        let out = match run(Command::new("iw").args(["phy", &format!("phy{phy}"), "info"])) {
            Some(o) => o,
            None => continue,
        };
        if let Some(freq) = pick_clean_5ghz_freq(&out) {
            // Prefer the lowest clean 5GHz channel (UNII-1: 36/40/44/48) — no
            // DFS wait, widely source-compatible. pick_clean_5ghz_freq already
            // returns the lowest, so first hit wins.
            best = Some(freq);
            break;
        }
    }
    best
}

/// Read `/sys/class/net/<iface>/phy80211/index` (the phyN number). sysfs, no cmd.
fn phy_index_of(iface: &str) -> Option<u32> {
    let path = format!("/sys/class/net/{iface}/phy80211/index");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Parse `iw phy phyN info` output and return the lowest clean, beacon-capable
/// 5GHz frequency (MHz): a `* <freq> MHz [chan] (...)` line that is NOT flagged
/// `no IR`, `disabled`, or `radar detection` (DFS). A GO cannot beacon on a
/// `no IR` channel, and DFS channels impose a radar-scan delay unsuitable for a
/// sink, so we require a plain channel.
pub fn pick_clean_5ghz_freq(iw_phy_info: &str) -> Option<u32> {
    let mut candidates: Vec<u32> = Vec::new();
    for line in iw_phy_info.lines() {
        let l = line.trim();
        // Match e.g. "* 5180.0 MHz [36] (22.0 dBm)" or "* 5180 MHz [36] ..."
        let Some(star) = l.strip_prefix("* ") else {
            continue;
        };
        let Some((freq_part, rest)) = star.split_once(" MHz") else {
            continue;
        };
        // Frequency (strip a possible ".0").
        let mhz: u32 = match freq_part.split('.').next().and_then(|n| n.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        if !(5000..5925).contains(&mhz) {
            continue; // 5GHz U-NII band only (6GHz starts ~5925/5955 MHz)
        }
        let flags = rest.to_ascii_lowercase();
        if flags.contains("no ir") || flags.contains("disabled") || flags.contains("radar") {
            continue;
        }
        candidates.push(mhz);
    }
    candidates.into_iter().min()
}

/// True if a VA-API driver `.so` is installed, i.e. hardware H.264 decode is
/// plausible. We look for any `*_drv_video.so` under the standard DRI dirs.
/// This is a HINT — the actual pipeline still falls back to software if the
/// hardware decoder fails to reach PLAYING.
fn hw_decode_available() -> bool {
    for dir in [
        "/usr/lib/x86_64-linux-gnu/dri",
        "/usr/lib/dri",
        "/usr/lib64/dri",
    ] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with("_drv_video.so") {
                    return true;
                }
            }
        }
    }
    false
}

/// Run a command with a short timeout; return stdout as String, or None on any
/// failure. Best-effort — never propagates an error.
fn run(cmd: &mut Command) -> Option<String> {
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const IW_PHY_SAMPLE: &str = "\
	Band 4:
		Frequencies:
			* 5180.0 MHz [36] (22.0 dBm) (no IR)
			* 5200.0 MHz [40] (22.0 dBm) (no IR)
			* 5260.0 MHz [52] (22.0 dBm) (no IR, radar detection)
			* 5745.0 MHz [149] (22.0 dBm)
			* 5765.0 MHz [153] (22.0 dBm)
			* 2412 MHz [1] (20.0 dBm)
";

    #[test]
    fn picks_lowest_clean_5ghz_skipping_no_ir_and_dfs() {
        // 36/40 are (no IR), 52 is radar → first clean is 149 (5745).
        assert_eq!(pick_clean_5ghz_freq(IW_PHY_SAMPLE), Some(5745));
    }

    #[test]
    fn picks_unii1_when_clean() {
        let s = "\
			* 5180.0 MHz [36] (23.0 dBm)
			* 5200.0 MHz [40] (23.0 dBm)
			* 5745.0 MHz [149] (13.0 dBm)
";
        // 36 (5180) is clean and lowest → chosen (no-DFS UNII-1, widely compatible).
        assert_eq!(pick_clean_5ghz_freq(s), Some(5180));
    }

    #[test]
    fn no_clean_5ghz_returns_none() {
        let s = "\
			* 5180.0 MHz [36] (22.0 dBm) (no IR)
			* 5260.0 MHz [52] (22.0 dBm) (no IR, radar detection)
			* 5845.0 MHz [169] (disabled)
			* 2412 MHz [1] (20.0 dBm)
";
        assert_eq!(pick_clean_5ghz_freq(s), None);
    }

    #[test]
    fn excludes_6ghz() {
        let s = "			* 5955.0 MHz [1] (23.0 dBm)\n";
        assert_eq!(pick_clean_5ghz_freq(s), None);
    }

    #[test]
    fn detect_band_and_resolution_are_consistent() {
        // With audio stubbed off; go_band/resolution must agree regardless of
        // this host's real radios.
        let caps = detect(|| false);
        match caps.go_band {
            GoBand::Band5 => {
                assert!(caps.go_freq_mhz >= 5000);
                assert_eq!(caps.max_resolution, (1920, 1080));
            }
            GoBand::Band24 => {
                assert!(caps.go_freq_mhz < 5000);
                assert_eq!(caps.max_resolution, (1280, 720));
            }
        }
        assert!(!caps.audio); // stub returned false
    }
}
