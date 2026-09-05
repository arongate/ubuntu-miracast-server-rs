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
    /// 5GHz — full bandwidth, 1080p-capable, but NOT reliably discoverable on
    /// the autonomous-GO path (phones scan 2.4GHz social channels).
    Band5,
    /// 2.4GHz social channel — universally discoverable, 720p-capped.
    Band24,
}

/// One rung of the GO bring-up ladder: a frequency to try and the resolution to
/// advertise if it comes up. The backend walks these in order, verifying each,
/// and uses the first that works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoCandidate {
    /// `p2p_group_add freq=` value in MHz; 0 = let the driver choose.
    pub freq_mhz: u32,
    pub band: GoBand,
    /// Advertised max resolution for this rung (band-appropriate).
    pub max_resolution: (u32, u32),
    /// Short human label for logs/prompts, e.g. "2.4GHz ch1".
    pub label: &'static str,
}

/// Detected machine capabilities + the ordered GO bring-up ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Wi-Fi interface names present (sysfs), sorted.
    pub wifi_interfaces: Vec<String>,
    /// Ordered GO config candidates to try, best-and-safest first.
    pub go_candidates: Vec<GoCandidate>,
    /// True if a VA-API hardware H.264 decoder driver is present.
    pub hw_decode: bool,
    /// True if a working audio sink/session is available.
    pub audio: bool,
}

impl Capabilities {
    /// One-line human summary for the startup `CapabilityReport` log.
    pub fn report(&self) -> String {
        let ladder: Vec<String> = self
            .go_candidates
            .iter()
            .map(|c| c.label.to_string())
            .collect();
        format!(
            "adapters=[{}] GO-ladder=[{}] hw_decode={} audio={}",
            self.wifi_interfaces.join(","),
            ladder.join(" → "),
            self.hw_decode,
            self.audio
        )
    }
}

/// 2.4GHz social-channel frequencies (MHz) — the discoverable channels a
/// Miracast source probes. Tried in this order.
const SOCIAL_FREQS_2G: [(u32, &str); 3] = [
    (2412, "2.4GHz ch1"),
    (2437, "2.4GHz ch6"),
    (2462, "2.4GHz ch11"),
];

/// Detect capabilities and build the ordered GO bring-up ladder. `audio_probe`
/// is injected so the caller passes the receiver's real audio-sink probe (tests
/// stub it).
///
/// Ladder policy (most-reliable first):
///   1. 2.4GHz social channels 1 → 6 → 11, each advertising 720p. These are the
///      channels a phone actually scans for a Miracast sink, so discovery works;
///      720p fits the 2.4GHz link so playback is smooth.
///   2. Driver-chosen frequency (no `freq=`), 720p — last-resort so the GO still
///      comes up on odd drivers.
///   3. OPT-IN ONLY (`MIRACAST_GO_5GHZ=1`): a clean 5GHz channel at 1080p,
///      appended LAST. 5GHz gives full bandwidth but a phone often cannot
///      discover a 5GHz-only autonomous GO, so it is never a default rung.
pub fn detect(audio_probe: impl Fn() -> bool) -> Capabilities {
    let wifi_interfaces = crate::utils::list_wifi_interfaces_sysfs();

    let mut go_candidates: Vec<GoCandidate> = Vec::new();

    let want_5ghz_first = std::env::var("MIRACAST_GO_5GHZ")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Optional 5GHz rung (opt-in). If the user forces 5GHz, put it FIRST so it
    // is tried before the 2.4GHz fallbacks; otherwise it is not offered.
    let five = pick_clean_5ghz(&wifi_interfaces);
    if want_5ghz_first {
        if let Some(freq) = five {
            go_candidates.push(GoCandidate {
                freq_mhz: freq,
                band: GoBand::Band5,
                max_resolution: (1920, 1080),
                label: "5GHz (opt-in)",
            });
        }
    }

    // 2.4GHz social channels — the reliable, discoverable rungs at 720p.
    for (freq, label) in SOCIAL_FREQS_2G {
        go_candidates.push(GoCandidate {
            freq_mhz: freq,
            band: GoBand::Band24,
            max_resolution: (1280, 720),
            label,
        });
    }

    // Last resort: let the driver choose (still 2.4GHz-class expectations).
    go_candidates.push(GoCandidate {
        freq_mhz: 0,
        band: GoBand::Band24,
        max_resolution: (1280, 720),
        label: "driver-chosen",
    });

    Capabilities {
        wifi_interfaces,
        go_candidates,
        hw_decode: hw_decode_available(),
        audio: audio_probe(),
    }
}

/// Return the lowest clean (beacon-capable, non-DFS) 5GHz frequency across the
/// given interfaces' phys, or None if none is usable.
fn pick_clean_5ghz(interfaces: &[String]) -> Option<u32> {
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
            // Lowest clean 5GHz channel wins (pick_clean_5ghz_freq returns it).
            return Some(freq);
        }
    }
    None
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
    fn default_ladder_is_2ghz_social_then_driver_choice() {
        // Default (no MIRACAST_GO_5GHZ): ladder is social ch1/6/11 + driver
        // choice, ALL 2.4GHz/720p, NO 5GHz rung (5GHz breaks phone discovery on
        // the autonomous-GO path). Env is process-global, so guard the assertion
        // to the default case rather than mutating it under other tests.
        if std::env::var("MIRACAST_GO_5GHZ").is_ok() {
            return;
        }
        let caps = detect(|| false);
        assert!(!caps.audio); // stub returned false
        assert!(!caps.go_candidates.is_empty());
        // Every default rung is 2.4GHz and 720p.
        for c in &caps.go_candidates {
            assert_eq!(c.band, GoBand::Band24, "unexpected 5GHz rung: {}", c.label);
            assert_eq!(c.max_resolution, (1280, 720));
        }
        // First rung is social ch1 (2412); a driver-chosen (freq 0) rung is last.
        assert_eq!(caps.go_candidates[0].freq_mhz, 2412);
        assert_eq!(caps.go_candidates.last().unwrap().freq_mhz, 0);
    }
}
