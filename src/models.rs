//! Data models for the Ubuntu Miracast Server.
//!
//! Faithful port of `src/miracast_server/models.py`. Validation happens at
//! construction time via `try_new`, mirroring the Python `__post_init__`
//! behaviour: a `ValidationError` carries the same `field: reason` message.

use chrono::{DateTime, Duration, Local};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// Raised when a model field fails validation (equivalent to Python ValueError).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError(pub String);

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ValidationError {}

fn mac_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$").unwrap())
}

fn validate_mac_address(value: &str, field_name: &str) -> Result<(), ValidationError> {
    if !mac_re().is_match(value) {
        return Err(ValidationError(format!(
            "{field_name}: must be a valid MAC address in XX:XX:XX:XX:XX:XX format"
        )));
    }
    Ok(())
}

fn validate_ipv4_address(value: &str, field_name: &str) -> Result<(), ValidationError> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 4 {
        return Err(ValidationError(format!(
            "{field_name}: must be a valid IPv4 address in dotted-decimal notation"
        )));
    }
    for part in parts {
        // No leading zeros allowed (except "0" itself).
        if part.len() > 1 && part.starts_with('0') {
            return Err(ValidationError(format!(
                "{field_name}: must be a valid IPv4 address (no leading zeros in octets)"
            )));
        }
        let octet: i64 = part.parse().map_err(|_| {
            ValidationError(format!(
                "{field_name}: must be a valid IPv4 address in dotted-decimal notation"
            ))
        })?;
        if !(0..=255).contains(&octet) {
            return Err(ValidationError(format!(
                "{field_name}: must be a valid IPv4 address (octets must be 0-255)"
            )));
        }
    }
    Ok(())
}

fn validate_group_interface(value: &str, field_name: &str) -> Result<(), ValidationError> {
    if value.len() < 2 || value.len() > 16 {
        return Err(ValidationError(format!(
            "{field_name}: must be between 2 and 16 characters"
        )));
    }
    Ok(())
}

fn validate_connected_at(
    value: DateTime<Local>,
    field_name: &str,
) -> Result<(), ValidationError> {
    let now = Local::now();
    let tolerance = Duration::seconds(1);
    if value > now + tolerance {
        return Err(ValidationError(format!(
            "{field_name}: must not be in the future"
        )));
    }
    Ok(())
}

/// Represents a connected Miracast source. Validated on construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingConnection {
    pub peer_address: String,
    pub peer_ip: String,
    pub peer_name: String,
    pub group_interface: String,
    pub our_ip: String,
    pub connected_at: DateTime<Local>,
    pub go_role: bool,
}

impl IncomingConnection {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        peer_address: impl Into<String>,
        peer_ip: impl Into<String>,
        peer_name: impl Into<String>,
        group_interface: impl Into<String>,
        our_ip: impl Into<String>,
        connected_at: DateTime<Local>,
        go_role: bool,
    ) -> Result<Self, ValidationError> {
        let peer_address = peer_address.into();
        let peer_ip = peer_ip.into();
        let group_interface = group_interface.into();
        let our_ip = our_ip.into();

        validate_mac_address(&peer_address, "peer_address")?;
        validate_ipv4_address(&peer_ip, "peer_ip")?;
        validate_group_interface(&group_interface, "group_interface")?;
        validate_ipv4_address(&our_ip, "our_ip")?;
        validate_connected_at(connected_at, "connected_at")?;

        Ok(Self {
            peer_address,
            peer_ip,
            peer_name: peer_name.into(),
            group_interface,
            our_ip,
            connected_at,
            go_role,
        })
    }
}

fn validate_non_negative_int(value: i64, field_name: &str) -> Result<(), ValidationError> {
    if value < 0 {
        return Err(ValidationError(format!(
            "{field_name}: must be a non-negative integer"
        )));
    }
    Ok(())
}

fn validate_frames(decoded: i64, dropped: i64) -> Result<(), ValidationError> {
    if decoded < 0 {
        return Err(ValidationError(
            "frames_decoded: must be a non-negative integer".to_string(),
        ));
    }
    if dropped < 0 {
        return Err(ValidationError(
            "frames_dropped: must be a non-negative integer".to_string(),
        ));
    }
    if decoded < dropped {
        return Err(ValidationError(
            "frames_decoded: must be greater than or equal to frames_dropped".to_string(),
        ));
    }
    Ok(())
}

/// Statistics for a receiving session. Validated on construction.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverStats {
    pub start_time: DateTime<Local>,
    pub end_time: Option<DateTime<Local>>,
    pub duration: i64,
    pub data_received: i64,
    pub average_bitrate: f64,
    pub peak_bitrate: f64,
    pub frames_decoded: i64,
    pub frames_dropped: i64,
    pub errors: i64,
    pub resolution: (u32, u32),
    pub codec: String,
}

impl Default for ReceiverStats {
    fn default() -> Self {
        Self {
            start_time: Local::now(),
            end_time: None,
            duration: 0,
            data_received: 0,
            average_bitrate: 0.0,
            peak_bitrate: 0.0,
            frames_decoded: 0,
            frames_dropped: 0,
            errors: 0,
            resolution: (0, 0),
            codec: String::new(),
        }
    }
}

impl ReceiverStats {
    /// Validate the invariants enforced by the Python `__post_init__`.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_non_negative_int(self.duration, "duration")?;
        validate_non_negative_int(self.data_received, "data_received")?;
        validate_frames(self.frames_decoded, self.frames_dropped)?;
        Ok(())
    }
}

/// Information about a Miracast source device.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceInfo {
    pub name: String,
    pub address: String,
    pub model: String,
    pub resolution: (u32, u32),
    pub codec: String,
    pub audio_codec: String,
}

/// Complete record of a receiving session, with JSON (de)serialization.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSessionRecord {
    pub source_info: SourceInfo,
    pub stats: ReceiverStats,
    pub timestamp: DateTime<Local>,
}

impl ServerSessionRecord {
    /// Serialize to a JSON-compatible value. Datetimes are ISO 8601 strings,
    /// `end_time` becomes JSON null when absent, resolution tuples become lists.
    pub fn to_dict(&self) -> Value {
        json!({
            "source_info": {
                "name": self.source_info.name,
                "address": self.source_info.address,
                "model": self.source_info.model,
                "resolution": [self.source_info.resolution.0, self.source_info.resolution.1],
                "codec": self.source_info.codec,
                "audio_codec": self.source_info.audio_codec,
            },
            "stats": {
                "start_time": iso(&self.stats.start_time),
                "end_time": self.stats.end_time.as_ref().map(iso),
                "duration": self.stats.duration,
                "data_received": self.stats.data_received,
                "average_bitrate": self.stats.average_bitrate,
                "peak_bitrate": self.stats.peak_bitrate,
                "frames_decoded": self.stats.frames_decoded,
                "frames_dropped": self.stats.frames_dropped,
                "errors": self.stats.errors,
                "resolution": [self.stats.resolution.0, self.stats.resolution.1],
                "codec": self.stats.codec,
            },
            "timestamp": iso(&self.timestamp),
        })
    }

    /// Deserialize from a JSON value. Raises `ValidationError` if required
    /// fields are missing or values are unparseable (no partial objects).
    pub fn from_dict(data: &Value) -> Result<Self, ValidationError> {
        let obj = data
            .as_object()
            .ok_or_else(|| ValidationError("data must be a dictionary".to_string()))?;

        for key in ["source_info", "stats", "timestamp"] {
            if !obj.contains_key(key) {
                return Err(ValidationError(format!("missing required field: {key}")));
            }
        }

        // source_info
        let si = obj["source_info"]
            .as_object()
            .ok_or_else(|| ValidationError("source_info must be a dictionary".to_string()))?;
        for key in ["name", "address", "model"] {
            if !si.contains_key(key) {
                return Err(ValidationError(format!(
                    "source_info missing required field: {key}"
                )));
            }
        }
        let source_info = SourceInfo {
            name: as_string(&si["name"]),
            address: as_string(&si["address"]),
            model: as_string(&si["model"]),
            resolution: as_resolution(si.get("resolution")),
            codec: si.get("codec").map(as_string).unwrap_or_default(),
            audio_codec: si.get("audio_codec").map(as_string).unwrap_or_default(),
        };

        // stats
        let st = obj["stats"]
            .as_object()
            .ok_or_else(|| ValidationError("stats must be a dictionary".to_string()))?;
        for key in [
            "start_time",
            "duration",
            "data_received",
            "frames_decoded",
            "frames_dropped",
        ] {
            if !st.contains_key(key) {
                return Err(ValidationError(format!(
                    "stats missing required field: {key}"
                )));
            }
        }
        let start_time = parse_iso(&as_string(&st["start_time"]))?;
        let end_time = match st.get("end_time") {
            Some(Value::Null) | None => None,
            Some(v) => Some(parse_iso(&as_string(v))?),
        };
        let stats = ReceiverStats {
            start_time,
            end_time,
            duration: as_i64(&st["duration"]),
            data_received: as_i64(&st["data_received"]),
            average_bitrate: st.get("average_bitrate").map(as_f64).unwrap_or(0.0),
            peak_bitrate: st.get("peak_bitrate").map(as_f64).unwrap_or(0.0),
            frames_decoded: as_i64(&st["frames_decoded"]),
            frames_dropped: as_i64(&st["frames_dropped"]),
            errors: st.get("errors").map(as_i64).unwrap_or(0),
            resolution: as_resolution(st.get("resolution")),
            codec: st.get("codec").map(as_string).unwrap_or_default(),
        };

        let timestamp = parse_iso(&as_string(&obj["timestamp"]))?;

        Ok(Self {
            source_info,
            stats,
            timestamp,
        })
    }
}

fn iso(dt: &DateTime<Local>) -> String {
    // Matches Python datetime.isoformat() (local, with microseconds + offset).
    dt.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string()
}

fn parse_iso(s: &str) -> Result<DateTime<Local>, ValidationError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Local))
        .map_err(|e| {
            ValidationError(format!("failed to deserialize ServerSessionRecord: {e}"))
        })
}

fn as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn as_i64(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn as_f64(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn as_resolution(v: Option<&Value>) -> (u32, u32) {
    match v.and_then(|v| v.as_array()) {
        Some(arr) if arr.len() >= 2 => (
            arr[0].as_u64().unwrap_or(0) as u32,
            arr[1].as_u64().unwrap_or(0) as u32,
        ),
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_connection_constructs() {
        let c = IncomingConnection::try_new(
            "00:11:22:33:44:55",
            "192.168.173.80",
            "phone",
            "p2p-wlan0-0",
            "192.168.173.1",
            Local::now(),
            true,
        );
        assert!(c.is_ok());
    }

    #[test]
    fn bad_mac_rejected() {
        let c = IncomingConnection::try_new(
            "not-a-mac",
            "192.168.173.80",
            "phone",
            "p2p-wlan0-0",
            "192.168.173.1",
            Local::now(),
            true,
        );
        assert!(c.unwrap_err().0.starts_with("peer_address:"));
    }

    #[test]
    fn leading_zero_octet_rejected() {
        let c = IncomingConnection::try_new(
            "00:11:22:33:44:55",
            "192.168.173.080",
            "phone",
            "p2p-wlan0-0",
            "192.168.173.1",
            Local::now(),
            true,
        );
        assert!(c.unwrap_err().0.contains("no leading zeros"));
    }

    #[test]
    fn future_timestamp_rejected() {
        let future = Local::now() + Duration::seconds(60);
        let c = IncomingConnection::try_new(
            "00:11:22:33:44:55",
            "192.168.173.80",
            "phone",
            "p2p-wlan0-0",
            "192.168.173.1",
            future,
            true,
        );
        assert!(c.unwrap_err().0.contains("must not be in the future"));
    }

    #[test]
    fn frames_decoded_lt_dropped_rejected() {
        let s = ReceiverStats {
            frames_decoded: 5,
            frames_dropped: 10,
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn record_roundtrips_through_json() {
        let rec = ServerSessionRecord {
            source_info: SourceInfo {
                name: "phone".into(),
                address: "00:11:22:33:44:55".into(),
                model: "Pixel".into(),
                resolution: (1920, 1080),
                codec: "H264".into(),
                audio_codec: "AAC".into(),
            },
            stats: ReceiverStats {
                duration: 42,
                data_received: 12345,
                frames_decoded: 100,
                frames_dropped: 2,
                resolution: (1920, 1080),
                codec: "H264".into(),
                ..Default::default()
            },
            timestamp: Local::now(),
        };
        let v = rec.to_dict();
        let back = ServerSessionRecord::from_dict(&v).unwrap();
        assert_eq!(back.source_info, rec.source_info);
        assert_eq!(back.stats.duration, 42);
        assert_eq!(back.stats.frames_dropped, 2);
    }

    #[test]
    fn from_dict_missing_field_errors() {
        let v = json!({"source_info": {}, "stats": {}});
        assert!(ServerSessionRecord::from_dict(&v).is_err());
    }
}
