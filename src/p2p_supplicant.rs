//! Dedicated wpa_supplicant instance manager for P2P operations.
//!
//! Manages a separate wpa_supplicant process for a dedicated Wi-Fi adapter,
//! enabling simultaneous internet (primary adapter) and Miracast P2P
//! (dedicated adapter) without channel conflicts.
//!
//! Faithful port of `src/miracast_server/p2p_supplicant.py`.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// wpa_supplicant config template — byte-identical to the Python
/// `_WPA_CONF_TEMPLATE` (only the two `{}` fields are substituted).
const WPA_CONF_TEMPLATE: &str = "\
ctrl_interface={ctrl_dir}
update_config=1
device_name={device_name}
device_type=7-0050F204-1
p2p_go_intent=1
driver_param=p2p_device=1
country=FR
";

// Default paths (intentional /tmp use, as in the Python source).
const CTRL_DIR: &str = "/tmp/miracast-wpa-p2p";
const CONF_PATH: &str = "/tmp/miracast-wpa-p2p.conf";
const LOG_PATH: &str = "/tmp/miracast-wpa-p2p.log";

/// Errors from supplicant lifecycle (equivalent to Python RuntimeError).
#[derive(Debug)]
pub struct SupplicantError(pub String);

impl std::fmt::Display for SupplicantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SupplicantError {}

/// Manages a dedicated wpa_supplicant instance for P2P on a secondary adapter.
pub struct P2PSupplicantManager {
    interface: String,
    device_name: String,
    process: Option<Child>,
    ctrl_dir: String,
    conf_path: String,
    log_path: String,
    was_nm_managed: bool,
    started: bool,
}

impl P2PSupplicantManager {
    pub fn new(interface: impl Into<String>, device_name: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            device_name: device_name.into(),
            process: None,
            ctrl_dir: CTRL_DIR.to_string(),
            conf_path: CONF_PATH.to_string(),
            log_path: LOG_PATH.to_string(),
            was_nm_managed: false,
            started: false,
        }
    }

    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// The wpa_supplicant control-socket directory.
    /// Use with: `wpa_cli -p <ctrl_path> -i <interface> <command>`.
    pub fn ctrl_path(&self) -> &str {
        &self.ctrl_dir
    }

    /// Whether the dedicated process is running.
    pub fn is_running(&mut self) -> bool {
        match self.process.as_mut() {
            None => false,
            Some(child) => matches!(child.try_wait(), Ok(None)),
        }
    }

    /// Start the dedicated wpa_supplicant instance.
    pub fn start(&mut self) -> Result<(), SupplicantError> {
        if self.started {
            log::debug!("P2P supplicant already started");
            return Ok(());
        }
        log::info!("Starting dedicated wpa_supplicant on {}", self.interface);

        // Kill any stale wpa_supplicant on this interface from a previous crash.
        let _ = run_brief(
            Command::new("sudo").args([
                "pkill",
                "-f",
                &format!("wpa_supplicant.*-i.*{}", self.interface),
            ]),
            Duration::from_secs(5),
        );
        std::thread::sleep(Duration::from_millis(500));

        self.unmanage_from_nm();
        self.write_config()?;

        // Create control socket directory (0755).
        if let Err(e) = std::fs::create_dir_all(&self.ctrl_dir) {
            return Err(SupplicantError(format!(
                "Failed to create ctrl dir: {e}"
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &self.ctrl_dir,
                std::fs::Permissions::from_mode(0o755),
            );
        }

        // Spawn wpa_supplicant with identical argv.
        let child = Command::new("sudo")
            .args([
                "wpa_supplicant",
                "-i",
                &self.interface,
                "-c",
                &self.conf_path,
                "-D",
                "nl80211",
                "-f",
                &self.log_path,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(c) => self.process = Some(c),
            Err(e) => {
                self.remanage_with_nm();
                return Err(SupplicantError(format!(
                    "Failed to start wpa_supplicant: {e}"
                )));
            }
        }

        if !self.wait_for_socket(Duration::from_secs(5)) {
            self.stop();
            return Err(SupplicantError(format!(
                "wpa_supplicant started but control socket not created within 5 seconds. \
                 Check {} for errors.",
                self.log_path
            )));
        }

        self.started = true;
        let pid = self.process.as_ref().map(|c| c.id()).unwrap_or(0);
        log::info!(
            "Dedicated wpa_supplicant running on {} (PID {}, ctrl={})",
            self.interface,
            pid,
            self.ctrl_dir
        );
        Ok(())
    }

    /// Stop the dedicated wpa_supplicant and restore NM management. Idempotent.
    pub fn stop(&mut self) {
        if !self.started && self.process.is_none() {
            return;
        }
        log::info!("Stopping dedicated wpa_supplicant on {}", self.interface);

        if let Some(mut child) = self.process.take() {
            if matches!(child.try_wait(), Ok(None)) {
                // Runs as root → SIGTERM via sudo kill, then SIGKILL fallback.
                let _ = run_brief(
                    Command::new("sudo").args(["kill", &child.id().to_string()]),
                    Duration::from_secs(5),
                );
                if !wait_child(&mut child, Duration::from_secs(5)) {
                    let _ = run_brief(
                        Command::new("sudo").args(["kill", "-9", &child.id().to_string()]),
                        Duration::from_secs(3),
                    );
                    let _ = child.wait();
                }
            }
        }

        self.started = false;
        self.remanage_with_nm();
        self.cleanup_files();
        log::info!("Dedicated wpa_supplicant stopped, NM management restored");
    }

    /// Run a wpa_cli command against the dedicated instance.
    pub fn run_wpa_cli(&self, args: &[&str]) -> Result<String, SupplicantError> {
        let mut cmd = Command::new("sudo");
        cmd.arg("wpa_cli")
            .arg("-p")
            .arg(&self.ctrl_dir)
            .arg("-i")
            .arg(&self.interface);
        for a in args {
            cmd.arg(a);
        }
        match run_brief(&mut cmd, Duration::from_secs(10)) {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if !out.status.success() {
                    Err(SupplicantError(format!(
                        "wpa_cli command failed: {}: {}",
                        args.join(" "),
                        if !stderr.is_empty() { stderr } else { stdout }
                    )))
                } else {
                    Ok(stdout)
                }
            }
            Err(_) => Err(SupplicantError(format!(
                "wpa_cli command timed out: {}",
                args.join(" ")
            ))),
        }
    }

    fn unmanage_from_nm(&mut self) {
        match run_brief(
            Command::new("nmcli").args(["device", "set", &self.interface, "managed", "no"]),
            Duration::from_secs(10),
        ) {
            Ok(out) if out.status.success() => {
                self.was_nm_managed = true;
                log::debug!("Unmanaged {} from NetworkManager", self.interface);
            }
            Ok(out) => log::debug!(
                "nmcli unmanage returned {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => log::debug!("Could not unmanage from NM: {e}"),
        }
    }

    fn remanage_with_nm(&mut self) {
        if !self.was_nm_managed {
            return;
        }
        let _ = run_brief(
            Command::new("nmcli").args(["device", "set", &self.interface, "managed", "yes"]),
            Duration::from_secs(10),
        );
        log::debug!("Restored NM management of {}", self.interface);
    }

    fn write_config(&self) -> Result<(), SupplicantError> {
        let content = WPA_CONF_TEMPLATE
            .replace("{ctrl_dir}", &self.ctrl_dir)
            .replace("{device_name}", &self.device_name);

        // Remove stale file from a previous run (may be root-owned).
        if Path::new(&self.conf_path).exists()
            && std::fs::remove_file(&self.conf_path).is_err() {
                let _ = run_brief(
                    Command::new("sudo").args(["rm", "-f", &self.conf_path]),
                    Duration::from_secs(5),
                );
            }
        // Clean stale control socket directory.
        if Path::new(&self.ctrl_dir).exists()
            && std::fs::remove_dir_all(&self.ctrl_dir).is_err() {
                let _ = run_brief(
                    Command::new("sudo").args(["rm", "-rf", &self.ctrl_dir]),
                    Duration::from_secs(5),
                );
            }

        std::fs::write(&self.conf_path, content)
            .map_err(|e| SupplicantError(format!("Failed to write wpa_supplicant config: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &self.conf_path,
                std::fs::Permissions::from_mode(0o644),
            );
        }
        log::debug!("Wrote wpa_supplicant config to {}", self.conf_path);
        Ok(())
    }

    fn wait_for_socket(&mut self, timeout: Duration) -> bool {
        let socket_path = Path::new(&self.ctrl_dir).join(&self.interface);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if socket_path.exists() {
                return true;
            }
            if let Some(child) = self.process.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    log::error!(
                        "wpa_supplicant exited with code {}",
                        status.code().unwrap_or(-1)
                    );
                    return false;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    fn cleanup_files(&self) {
        let _ = std::fs::remove_file(&self.conf_path);
        let socket_path = Path::new(&self.ctrl_dir).join(&self.interface);
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir(&self.ctrl_dir);
    }
}

impl Drop for P2PSupplicantManager {
    fn drop(&mut self) {
        // Best-effort cleanup so a dropped manager does not leak a root process.
        if self.started || self.process.is_some() {
            self.stop();
        }
    }
}

/// Run a command with a wall-clock timeout (std has none). On timeout, kill.
fn run_brief(cmd: &mut Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    use std::io::Read;
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
                return Ok(std::process::Output { status, stdout, stderr });
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

fn wait_child(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_template_is_byte_identical() {
        let m = P2PSupplicantManager::new("wlan0", "Ubuntu Miracast Server");
        let content = WPA_CONF_TEMPLATE
            .replace("{ctrl_dir}", m.ctrl_path())
            .replace("{device_name}", "Ubuntu Miracast Server");
        let expected = "ctrl_interface=/tmp/miracast-wpa-p2p\n\
                        update_config=1\n\
                        device_name=Ubuntu Miracast Server\n\
                        device_type=7-0050F204-1\n\
                        p2p_go_intent=1\n\
                        driver_param=p2p_device=1\n\
                        country=FR\n";
        assert_eq!(content, expected);
    }

    #[test]
    fn ctrl_path_matches_python_default() {
        let m = P2PSupplicantManager::new("wlan0", "x");
        assert_eq!(m.ctrl_path(), "/tmp/miracast-wpa-p2p");
    }
}
