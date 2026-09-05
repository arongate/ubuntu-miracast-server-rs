//! Configuration management for Ubuntu Miracast Server.
//!
//! Faithful port of `src/miracast_server/config.py`: layered `serde_json`
//! config with per-key validation, atomic writes at 0600, and the same
//! default document.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Validation constraint for a known key.
struct Rule {
    is_int: bool,
    min: Option<i64>,
    max: Option<i64>,
}

fn validation_rule(section: &str, key: &str) -> Option<Rule> {
    match (section, key) {
        ("streaming", "rtsp_port") => Some(Rule { is_int: true, min: Some(1024), max: Some(65535) }),
        ("network", "go_intent") => Some(Rule { is_int: true, min: Some(0), max: Some(15) }),
        ("network", "connection_timeout") => Some(Rule { is_int: true, min: Some(1), max: Some(120) }),
        ("network", "rtp_port") => Some(Rule { is_int: true, min: Some(1024), max: Some(65535) }),
        _ => None,
    }
}

fn default_config() -> Value {
    json!({
        "general": {
            "device_name": "Ubuntu Miracast Server",
            "start_minimized": false,
            "fullscreen_on_stream": true,
            "log_level": "INFO"
        },
        "streaming": {
            "rtsp_port": 7236,
            "audio_enabled": true,
            "max_resolution": "1920x1080",
            "preferred_codec": "H264"
        },
        "network": {
            "go_intent": 15,
            "connection_timeout": 30,
            "auto_accept": true,
            "rtp_port": 1028,
            "p2p_interface": "",
            "listen_channel": 0
        },
        "display": {
            "preferred_resolution": "1920x1080",
            "show_stream_info": true,
            "hw_accel": true
        },
        "advanced": {
            "session_timeout": 30,
            "keep_alive_interval": 15,
            "buffer_size_ms": 100
        },
        "service": {
            "enabled": false,
            "virtual_display": false,
            "idle_timeout": 0
        }
    })
}

/// Manages server configuration with validation and JSON persistence.
pub struct ServerConfig {
    pub config_path: PathBuf,
    pub config: Value,
}

impl ServerConfig {
    /// Create a manager, loading `~/.config/ubuntu-miracast-server/config.json`
    /// (or `config_path` when given), creating defaults if absent/malformed.
    pub fn new(config_path: Option<&Path>) -> Self {
        let path = match config_path {
            Some(p) => p.to_path_buf(),
            None => default_config_path(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut cfg = Self {
            config_path: path,
            config: Value::Null,
        };
        cfg.config = cfg.load_config();
        cfg
    }

    fn load_config(&self) -> Value {
        if self.config_path.exists() {
            match std::fs::read_to_string(&self.config_path) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v.is_object() => v,
                    Ok(_) => {
                        log::warn!(
                            "Config file {} does not contain a JSON object, using defaults",
                            self.config_path.display()
                        );
                        self.create_default_config()
                    }
                    Err(e) => {
                        log::warn!(
                            "Malformed JSON in config file {}: {} — using defaults",
                            self.config_path.display(),
                            e
                        );
                        self.create_default_config()
                    }
                },
                Err(e) => {
                    log::error!(
                        "Failed to read config file {}: {}",
                        self.config_path.display(),
                        e
                    );
                    self.create_default_config()
                }
            }
        } else {
            self.create_default_config()
        }
    }

    fn create_default_config(&self) -> Value {
        let config = default_config();
        if let Err(e) = self.write_config(&config) {
            log::error!("Failed to save default config: {e}");
        }
        config
    }

    fn write_config(&self, config: &Value) -> std::io::Result<()> {
        write_json_0600(&self.config_path, config)
    }

    fn validate(&self, section: &str, key: &str, value: &Value) -> Result<(), String> {
        let rule = match validation_rule(section, key) {
            Some(r) => r,
            None => return Ok(()),
        };
        if rule.is_int {
            // Python isinstance(value, int) — reject bools and non-integers.
            let n = match value {
                Value::Bool(_) => {
                    return Err(format!("{section}.{key} must be int, got bool"))
                }
                Value::Number(num) if num.is_i64() || num.is_u64() => {
                    num.as_i64().unwrap()
                }
                other => {
                    return Err(format!(
                        "{section}.{key} must be int, got {}",
                        json_type_name(other)
                    ))
                }
            };
            if let Some(min) = rule.min {
                if n < min {
                    return Err(format!("{section}.{key} must be >= {min}, got {n}"));
                }
            }
            if let Some(max) = rule.max {
                if n > max {
                    return Err(format!("{section}.{key} must be <= {max}, got {n}"));
                }
            }
        }
        Ok(())
    }

    /// Get a configuration value, or `default` if the key is absent.
    pub fn get(&self, section: &str, key: &str, default: Value) -> Value {
        self.config
            .get(section)
            .and_then(|s| s.get(key))
            .cloned()
            .unwrap_or(default)
    }

    /// Convenience getter for string values.
    pub fn get_str(&self, section: &str, key: &str, default: &str) -> String {
        self.get(section, key, Value::String(default.to_string()))
            .as_str()
            .unwrap_or(default)
            .to_string()
    }

    /// Convenience getter for integer values.
    pub fn get_i64(&self, section: &str, key: &str, default: i64) -> i64 {
        self.get(section, key, json!(default))
            .as_i64()
            .unwrap_or(default)
    }

    /// Convenience getter for boolean values.
    pub fn get_bool(&self, section: &str, key: &str, default: bool) -> bool {
        self.get(section, key, json!(default))
            .as_bool()
            .unwrap_or(default)
    }

    /// Set a configuration value with validation. On disk-write failure the
    /// value is still retained in memory (matching the Python behaviour).
    pub fn set(&mut self, section: &str, key: &str, value: Value) -> Result<(), String> {
        self.validate(section, key, &value)?;

        let obj = self
            .config
            .as_object_mut()
            .expect("config root is always an object");
        let sect = obj
            .entry(section.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(sect_obj) = sect.as_object_mut() {
            sect_obj.insert(key.to_string(), value);
        }

        if let Err(e) = self.write_config(&self.config) {
            log::error!("Failed to persist config after setting {section}.{key}: {e}");
        }
        Ok(())
    }

    /// Save the current (or a replacement) configuration to disk.
    pub fn save(&mut self, config: Option<Value>) {
        if let Some(c) = config {
            self.config = c;
        }
        if let Err(e) = self.write_config(&self.config) {
            log::error!("Failed to save config: {e}");
        }
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

pub(crate) fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("ubuntu-miracast-server")
        .join("config.json")
}

/// Write `value` as pretty JSON to `path` atomically with 0600 permissions.
///
/// Writes to `<path>.tmp`, then renames. Mirrors the Python `_write_config`.
pub(crate) fn write_json_0600(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("tmp");

    let serialized = serde_json::to_string_pretty(value)
        .map_err(std::io::Error::other)?;

    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)?;
            f.write_all(serialized.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(serialized.as_bytes())?;
        }
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // Ensure final perms (rename may preserve prior mode).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = ServerConfig::new(Some(&path));
        assert_eq!(cfg.get_str("general", "device_name", "x"), "Ubuntu Miracast Server");
        assert_eq!(cfg.get_i64("streaming", "rtsp_port", 0), 7236);
        assert_eq!(cfg.get_i64("network", "rtp_port", 0), 1028);
        assert!(cfg.get_bool("streaming", "audio_enabled", false));
        assert!(path.exists());
    }

    #[test]
    fn set_validates_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = ServerConfig::new(Some(&path));
        assert!(cfg.set("network", "go_intent", json!(20)).is_err());
        assert!(cfg.set("network", "go_intent", json!(10)).is_ok());
        assert_eq!(cfg.get_i64("network", "go_intent", 0), 10);
    }

    #[test]
    fn set_rejects_bool_for_int_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = ServerConfig::new(Some(&path));
        assert!(cfg.set("streaming", "rtsp_port", json!(true)).is_err());
    }

    #[test]
    fn persisted_value_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        {
            let mut cfg = ServerConfig::new(Some(&path));
            cfg.set("network", "rtp_port", json!(2000)).unwrap();
        }
        let cfg2 = ServerConfig::new(Some(&path));
        assert_eq!(cfg2.get_i64("network", "rtp_port", 0), 2000);
    }

    #[test]
    #[cfg(unix)]
    fn file_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let _ = ServerConfig::new(Some(&path));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
