//! Core utility and shared helpers for the Miracast Server.
//!
//! Provides validated subprocess wrappers, codec whitelisting, and RTSP
//! security enforcement. All security-sensitive validation is centralized here.
//!
//! Faithful port of `src/miracast_server/utils.py`.

use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

use regex::Regex;

// Allowed characters for wpa_cli parameters:
// alphanumeric, colons, hyphens, underscores, dots, slashes, spaces, equals.
// '=' is required by legitimate key=value wpa_cli args (freq=2412,
// type=progressive) and is not a shell-metacharacter (see WPA_DANGEROUS_CHARS).
fn wpa_param_re() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9:\-_./ =]+$").unwrap())
}

const WPA_PARAM_MAX_LENGTH: usize = 256;

/// Characters that could be used for command injection in shell contexts.
const WPA_DANGEROUS_CHARS: &[char] = &[
    ';', '|', '`', '$', '&', '\n', '\r', '\0', '<', '>', '(', ')', '{', '}',
];

// ─── Codec and RTSP Security Constants ────────────────────────────────────────

/// Codec whitelist for pipeline construction (requirement 10.4).
pub const ALLOWED_VIDEO_CODECS: &[&str] = &["H264"];
pub const ALLOWED_AUDIO_CODECS: &[&str] = &["AAC"];

/// RTSP request size limits (requirement 10.7).
pub const RTSP_MAX_HEADER_SIZE: usize = 8192; // 8 KB
pub const RTSP_MAX_BODY_SIZE: usize = 65536; // 64 KB

/// Errors raised by wpa_cli parameter validation and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WpaError {
    /// Parameter failed the security allowlist (equivalent to Python ValueError).
    Value(String),
    /// The command failed, timed out, or could not be executed (RuntimeError).
    Runtime(String),
}

impl std::fmt::Display for WpaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WpaError::Value(m) => write!(f, "{m}"),
            WpaError::Runtime(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for WpaError {}

/// Validate a codec name against the whitelist.
pub fn validate_codec(codec: &str, codec_type: &str) -> bool {
    match codec_type {
        "video" => ALLOWED_VIDEO_CODECS.contains(&codec),
        "audio" => ALLOWED_AUDIO_CODECS.contains(&codec),
        _ => false,
    }
}

/// Validate RTSP message sizes against security limits.
pub fn validate_rtsp_size(header_bytes: usize, body_bytes: usize) -> bool {
    header_bytes <= RTSP_MAX_HEADER_SIZE && body_bytes <= RTSP_MAX_BODY_SIZE
}

/// Validate a network port number (range 1024-65535).
pub fn validate_port(port: i64) -> bool {
    (1024..=65535).contains(&port)
}

/// Information about a discovered P2P interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceInfo {
    pub interface: String,
    pub parent: String,
    pub driver: String,
    pub status: String,
}

/// Enumerate all Wi-Fi-capable network interfaces from sysfs, regardless of
/// their up/down or NetworkManager-managed state.
///
/// This is the authoritative, state-independent source for picking a dedicated
/// P2P adapter: the ideal adapter is an idle USB dongle that NetworkManager is
/// NOT managing — which is exactly why it is absent from wpa_supplicant's D-Bus
/// `Interfaces` list and shows as `unmanaged` in nmcli. sysfs lists every
/// interface that has a `phy80211` (i.e. is a real 802.11 netdev), including
/// down/unmanaged ones, so selection can still find and then bring it up.
/// Excludes `p2p-*` group/child interfaces (they are transient GO netdevs, not
/// physical adapters).
pub fn list_wifi_interfaces_sysfs() -> Vec<String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir("/sys/class/net") {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Physical Wi-Fi netdev = has a phy80211 symlink; skip P2P group ifaces.
        if name.starts_with("p2p-") {
            continue;
        }
        if entry.path().join("phy80211").exists() {
            out.push(name);
        }
    }
    out.sort();
    out
}

/// Validate and sanitize a wpa_cli parameter.
///
/// Ensures the parameter does not contain shell metacharacters that could
/// enable command injection. This is a CRITICAL security boundary because
/// these parameters flow into subprocess calls that run with sudo.
pub fn validate_wpa_param(param: &str) -> Result<String, WpaError> {
    if param.is_empty() {
        return Err(WpaError::Value(
            "wpa_cli parameter must not be empty".to_string(),
        ));
    }

    if param.len() > WPA_PARAM_MAX_LENGTH {
        return Err(WpaError::Value(format!(
            "wpa_cli parameter too long ({} > {})",
            param.len(),
            WPA_PARAM_MAX_LENGTH
        )));
    }

    let dangerous: HashSet<char> = param
        .chars()
        .filter(|c| WPA_DANGEROUS_CHARS.contains(c))
        .collect();
    if !dangerous.is_empty() {
        // Match Python's set repr ordering loosely; content is what matters.
        return Err(WpaError::Value(format!(
            "wpa_cli parameter contains dangerous characters: {dangerous:?}"
        )));
    }

    if !wpa_param_re().is_match(param) {
        return Err(WpaError::Value(format!(
            "wpa_cli parameter contains invalid characters: {param:?}"
        )));
    }

    Ok(param.to_string())
}

/// Run a wpa_cli command with parameter validation.
///
/// All parameters are validated against an allowlist. Uses list-based
/// subprocess execution (no shell), exactly like the Python `_run_wpa_cli`.
///
/// * `skip_last_validation` — treat the final arg as a user-provided value
///   (e.g. device_name) that is not checked against the strict allowlist.
///   Still safe because we never invoke a shell.
/// * `ctrl_path` — optional control-socket dir for a dedicated wpa_supplicant
///   instance; passed via `-p` when present.
pub fn run_wpa_cli(
    interface: &str,
    args: &[&str],
    skip_last_validation: bool,
    ctrl_path: Option<&str>,
) -> Result<String, WpaError> {
    validate_wpa_param(interface)?;

    let to_validate = if skip_last_validation && !args.is_empty() {
        &args[..args.len() - 1]
    } else {
        args
    };
    for a in to_validate {
        validate_wpa_param(a)?;
    }

    // Build command as a list — no shell.
    let mut cmd = Command::new("sudo");
    cmd.arg("wpa_cli");
    if let Some(p) = ctrl_path {
        cmd.arg("-p").arg(p);
    }
    cmd.arg("-i").arg(interface);
    for a in args {
        cmd.arg(a);
    }

    let output = run_with_timeout(&mut cmd, Duration::from_secs(10))
        .map_err(|e| WpaError::Runtime(format!("Failed to execute wpa_cli: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        return Err(WpaError::Runtime(format!(
            "wpa_cli command failed with exit code {code}: {}",
            if !stderr.is_empty() { stderr } else { stdout }
        )));
    }
    Ok(stdout)
}

/// List all available P2P device interfaces on the system.
///
/// Tries wpa_supplicant's D-Bus interface first (no sudo, `netdev`-group-gated):
/// reads the root `Interfaces` property and each object's `Ifname`. Only if the
/// bus is unreachable or returns nothing does it fall back to the legacy sudo
/// `wpa_cli` + `nmcli` enumeration — so the D-Bus root-free path shells out to
/// nothing.
pub fn list_p2p_interfaces() -> Vec<InterfaceInfo> {
    let dbus = list_p2p_interfaces_dbus();
    if !dbus.is_empty() {
        return dbus;
    }
    list_p2p_interfaces_subprocess()
}

/// Sudo-free enumeration via `fi.w1.wpa_supplicant1` on the system bus.
/// Returns an empty vec if the bus is unreachable (caller falls back).
fn list_p2p_interfaces_dbus() -> Vec<InterfaceInfo> {
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::OwnedObjectPath;

    let mut out = Vec::new();
    let conn = match Connection::system() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let root = match Proxy::new(
        &conn,
        "fi.w1.wpa_supplicant1",
        "/fi/w1/wpa_supplicant1",
        "fi.w1.wpa_supplicant1",
    ) {
        Ok(p) => p,
        Err(_) => return out,
    };
    let ifaces: Vec<OwnedObjectPath> = match root.get_property("Interfaces") {
        Ok(v) => v,
        Err(_) => return out,
    };
    for obj in ifaces {
        let p = match Proxy::new(
            &conn,
            "fi.w1.wpa_supplicant1".to_string(),
            obj.as_str().to_string(),
            "fi.w1.wpa_supplicant1.Interface".to_string(),
        ) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Ok(name) = p.get_property::<String>("Ifname") {
            // Driver from sysfs (sudo-free); status is best-effort "available"
            // (the P2P-capability status probe would itself need sudo/nmcli).
            let driver = read_interface_driver(&name);
            out.push(InterfaceInfo {
                interface: name.clone(),
                parent: name,
                driver,
                status: "available".to_string(),
            });
        }
    }
    out
}

/// Legacy enumeration via sudo `wpa_cli` + `nmcli`. Fallback only.
fn list_p2p_interfaces_subprocess() -> Vec<InterfaceInfo> {
    let mut interfaces = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Method 1: wpa_supplicant interfaces.
    if let Ok(out) = run_with_timeout(
        Command::new("sudo").arg("wpa_cli").arg("interface"),
        Duration::from_secs(5),
    ) {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).trim().lines() {
                let line = line.trim();
                if let Some(parent) = line.strip_prefix("p2p-dev-") {
                    if seen.insert(line.to_string()) {
                        interfaces.push(get_interface_info(line, parent));
                    }
                }
            }
        }
    }

    // Method 2: NetworkManager wifi-p2p devices.
    if let Ok(out) = run_with_timeout(
        Command::new("nmcli").args(["-t", "-f", "DEVICE,TYPE,STATE", "device", "status"]),
        Duration::from_secs(5),
    ) {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).trim().lines() {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 3 && parts[1] == "wifi-p2p" {
                    let iface_name = parts[0];
                    if seen.insert(iface_name.to_string()) {
                        let parent = iface_name.strip_prefix("p2p-dev-").unwrap_or(iface_name);
                        interfaces.push(get_interface_info(iface_name, parent));
                    }
                }
            }
        }
    }

    interfaces
}

fn get_interface_info(iface_name: &str, parent: &str) -> InterfaceInfo {
    // Driver name from sysfs instead of spawning `ethtool -i`: the
    // /sys/class/net/<iface>/device/driver symlink points at the driver module,
    // and its basename is exactly what `ethtool -i` reports as "driver:".
    // Read-only, unprivileged, no subprocess.
    let driver = read_interface_driver(parent);

    let mut status = "available".to_string();
    if let Ok(out) = run_with_timeout(
        Command::new("nmcli").args(["-t", "-f", "DEVICE,STATE", "device", "status"]),
        Duration::from_secs(5),
    ) {
        if out.status.success() {
            for l in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(rest) = l.strip_prefix(&format!("{parent}:")) {
                    status = if rest.contains("connected") && !rest.contains("disconnected") {
                        "connected".to_string()
                    } else {
                        "disconnected".to_string()
                    };
                }
            }
        }
    }

    InterfaceInfo {
        interface: iface_name.to_string(),
        parent: parent.to_string(),
        driver,
        status,
    }
}

/// Read a network interface's driver name from sysfs.
///
/// `/sys/class/net/<iface>/device/driver` is a symlink to the driver module
/// directory; its final component is the driver name (e.g. `rtw89_8852cu`,
/// `iwlwifi`) — the same value `ethtool -i <iface>` prints as "driver:".
/// Returns an empty string if the interface has no bound driver.
fn read_interface_driver(iface: &str) -> String {
    let link = format!("/sys/class/net/{iface}/device/driver");
    std::fs::read_link(&link)
        .ok()
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Find the best P2P-capable interface for Miracast.
///
/// Returns `(p2p_interface, wifi_interface)`. Prefers a disconnected/dedicated
/// adapter over one already connected to a router, matching the Python logic.
pub fn find_p2p_interface() -> Result<(String, String), WpaError> {
    let out = run_with_timeout(
        Command::new("sudo").arg("wpa_cli").arg("interface"),
        Duration::from_secs(5),
    )
    .map_err(|e| {
        WpaError::Runtime(format!(
            "Failed to execute wpa_cli: {e}. Ensure wpa_supplicant is installed and accessible."
        ))
    })?;

    if !out.status.success() {
        return Err(WpaError::Runtime(
            "wpa_cli returned non-zero exit code. \
             Ensure wpa_supplicant is running with P2P support."
                .to_string(),
        ));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut p2p_dev: Vec<(String, String)> = Vec::new();
    let mut wifi: Vec<String> = Vec::new();
    for line in stdout.trim().lines() {
        let line = line.trim();
        if let Some(parent) = line.strip_prefix("p2p-dev-") {
            p2p_dev.push((line.to_string(), parent.to_string()));
        } else if line.starts_with("wl") {
            wifi.push(line.to_string());
        }
    }

    // Prefer a dedicated (disconnected) wl* adapter with P2P support.
    for wifi_iface in &wifi {
        if p2p_dev.iter().any(|(_, parent)| parent == wifi_iface) {
            continue;
        }
        if probe_realtek_p2p(wifi_iface) {
            return Ok((wifi_iface.clone(), wifi_iface.clone()));
        }
    }

    // Next: p2p-dev-* — prefer one whose parent is disconnected.
    if !p2p_dev.is_empty() {
        let mut best: Option<(String, String)> = None;
        let mut fallback: Option<(String, String)> = None;
        for (p2p_iface, parent) in &p2p_dev {
            match run_with_timeout(
                Command::new("sudo").args(["wpa_cli", "-i", parent, "status"]),
                Duration::from_secs(5),
            ) {
                Ok(status) if status.status.success() => {
                    let s = String::from_utf8_lossy(&status.stdout);
                    if s.contains("wpa_state=COMPLETED") {
                        fallback = Some((p2p_iface.clone(), parent.clone()));
                    } else {
                        best = Some((p2p_iface.clone(), parent.clone()));
                        break;
                    }
                }
                _ => fallback = Some((p2p_iface.clone(), parent.clone())),
            }
        }
        if let Some(chosen) = best.or(fallback) {
            return Ok(chosen);
        }
    }

    // Last fallback: any wl* with P2P support.
    for wifi_iface in &wifi {
        if p2p_dev.iter().any(|(_, parent)| parent == wifi_iface) {
            continue;
        }
        if probe_realtek_p2p(wifi_iface) {
            return Ok((wifi_iface.clone(), wifi_iface.clone()));
        }
    }

    Err(WpaError::Runtime(
        "No P2P-capable interface detected. \
         Ensure Wi-Fi is enabled and wpa_supplicant has P2P support."
            .to_string(),
    ))
}

/// Probe a wl* interface for Realtek-style P2P support and disconnected state.
fn probe_realtek_p2p(wifi_iface: &str) -> bool {
    let test = match run_with_timeout(
        Command::new("sudo").args(["wpa_cli", "-i", wifi_iface, "p2p_find", "1"]),
        Duration::from_secs(5),
    ) {
        Ok(t) => t,
        Err(_) => return false,
    };
    if !(test.status.success() && String::from_utf8_lossy(&test.stdout).contains("OK")) {
        return false;
    }
    // Stop the test find.
    let _ = run_with_timeout(
        Command::new("sudo").args(["wpa_cli", "-i", wifi_iface, "p2p_stop_find"]),
        Duration::from_secs(5),
    );
    // Prefer disconnected adapters.
    match run_with_timeout(
        Command::new("sudo").args(["wpa_cli", "-i", wifi_iface, "status"]),
        Duration::from_secs(5),
    ) {
        Ok(status) if status.status.success() => {
            !String::from_utf8_lossy(&status.stdout).contains("wpa_state=COMPLETED")
        }
        _ => false,
    }
}

/// Run a command with a wall-clock timeout, mirroring `subprocess.run(timeout=)`.
///
/// Rust's std has no built-in timeout, so we spawn and poll; on timeout we kill
/// the child and return an error, matching the Python TimeoutExpired handling
/// (callers treat any error as "command unavailable/failed").
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut o) = child.stdout.take() {
                    let _ = o.read_to_end(&mut stdout);
                }
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_end(&mut stderr);
                }
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "command timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_whitelist() {
        assert!(validate_codec("H264", "video"));
        assert!(!validate_codec("H265", "video"));
        assert!(validate_codec("AAC", "audio"));
        assert!(!validate_codec("MP3", "audio"));
        assert!(!validate_codec("H264", "bogus"));
    }

    #[test]
    fn port_range() {
        assert!(!validate_port(80));
        assert!(validate_port(1024));
        assert!(validate_port(7236));
        assert!(validate_port(65535));
        assert!(!validate_port(65536));
    }

    #[test]
    fn rtsp_size_limits() {
        assert!(validate_rtsp_size(8192, 65536));
        assert!(!validate_rtsp_size(8193, 0));
        assert!(!validate_rtsp_size(0, 65537));
    }

    #[test]
    fn wpa_param_accepts_safe() {
        assert!(validate_wpa_param("p2p-dev-wlan0").is_ok());
        assert!(validate_wpa_param("00:11:22:33:44:55").is_ok());
        assert!(validate_wpa_param("wfd_subelem_set").is_ok());
        assert!(validate_wpa_param("Ubuntu Miracast Server").is_ok());
        // key=value args used by p2p_group_add / p2p_find (regression: the '='
        // was missing from the allowlist and rejected freq=2412 in the field).
        assert!(validate_wpa_param("freq=2412").is_ok());
        assert!(validate_wpa_param("type=progressive").is_ok());
    }

    #[test]
    fn sysfs_wifi_enumeration_excludes_p2p_and_is_graceful() {
        // Never panics regardless of host; returns physical wifi netdevs only.
        let ifaces = list_wifi_interfaces_sysfs();
        assert!(
            ifaces.iter().all(|i| !i.starts_with("p2p-")),
            "p2p-* group interfaces must be excluded: {ifaces:?}"
        );
        // 'lo' is not a wifi netdev (no phy80211) → must not appear.
        assert!(!ifaces.iter().any(|i| i == "lo"));
    }

    #[test]
    fn interface_driver_read_is_graceful() {
        // A nonexistent interface has no sysfs driver symlink → empty string,
        // never a panic. (The positive path depends on host hardware, so we
        // only assert the graceful-miss behaviour deterministically.)
        assert_eq!(read_interface_driver("definitely-no-such-iface-xyz"), "");
        // `lo` exists but has no bound driver → also empty, still no panic.
        let _ = read_interface_driver("lo");
    }

    #[test]
    fn wpa_param_rejects_injection() {
        assert!(matches!(validate_wpa_param(""), Err(WpaError::Value(_))));
        assert!(matches!(
            validate_wpa_param("foo; rm -rf /"),
            Err(WpaError::Value(_))
        ));
        assert!(matches!(
            validate_wpa_param("foo`whoami`"),
            Err(WpaError::Value(_))
        ));
        assert!(matches!(
            validate_wpa_param("foo$(id)"),
            Err(WpaError::Value(_))
        ));
        assert!(matches!(
            validate_wpa_param("foo|bar"),
            Err(WpaError::Value(_))
        ));
        assert!(matches!(
            validate_wpa_param(&"a".repeat(257)),
            Err(WpaError::Value(_))
        ));
    }
}
