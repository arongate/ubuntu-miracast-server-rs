//! RTSP message parsing and generation for Miracast WFD sessions.
//!
//! Implements RTSP request parsing, response building, and WFD parameter
//! handling for the Miracast sink RTSP protocol flow.
//!
//! Faithful port of `src/miracast_server/rtsp.py`.

use std::collections::BTreeMap;

// RTSP size limits (security constraints)
const MAX_HEADER_SIZE: usize = 8192; // 8 KB max header block
const MAX_BODY_SIZE: usize = 65536; // 64 KB max body

/// Known RTSP methods used in WFD sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspMethod {
    Options = 1,
    GetParameter = 2,
    SetParameter = 3,
    Setup = 4,
    Play = 5,
    Teardown = 6,
    Pause = 7,
}

impl RtspMethod {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "OPTIONS" => Some(Self::Options),
            "GET_PARAMETER" => Some(Self::GetParameter),
            "SET_PARAMETER" => Some(Self::SetParameter),
            "SETUP" => Some(Self::Setup),
            "PLAY" => Some(Self::Play),
            "TEARDOWN" => Some(Self::Teardown),
            "PAUSE" => Some(Self::Pause),
            _ => None,
        }
    }
}

// RTSP status codes
pub const RTSP_OK: u16 = 200;
pub const RTSP_BAD_REQUEST: u16 = 400;
pub const RTSP_NOT_FOUND: u16 = 404;
pub const RTSP_METHOD_NOT_ALLOWED: u16 = 405;
pub const RTSP_REQUEST_ENTITY_TOO_LARGE: u16 = 413;
pub const RTSP_INTERNAL_SERVER_ERROR: u16 = 500;
pub const RTSP_NOT_IMPLEMENTED: u16 = 501;

fn status_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Request Entity Too Large",
        451 => "Parameter Not Understood",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

/// Raised when an RTSP message cannot be parsed. Carries a status code
/// (defaults to 400 Bad Request), matching the Python `RTSPParseError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspParseError {
    pub message: String,
    pub status_code: u16,
}

impl RtspParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status_code: RTSP_BAD_REQUEST,
        }
    }
    fn with_code(message: impl Into<String>, status_code: u16) -> Self {
        Self {
            message: message.into(),
            status_code,
        }
    }
}

impl std::fmt::Display for RtspParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for RtspParseError {}

/// Parsed RTSP request.
#[derive(Debug, Clone)]
pub struct RtspRequest {
    pub method: RtspMethod,
    pub uri: String,
    pub version: String,
    pub cseq: i64,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    pub content_length: i64,
}

/// RTSP response to be sent.
#[derive(Debug, Clone)]
pub struct RtspResponse {
    pub status_code: u16,
    pub cseq: i64,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl RtspResponse {
    pub fn status_phrase(&self) -> &'static str {
        status_phrase(self.status_code)
    }

    /// Serialize the response to bytes for sending over the wire.
    ///
    /// Header insertion order is preserved by iterating a `Vec` we control;
    /// the Python dict preserved insertion order, and callers only insert a
    /// small fixed set, so ordering fidelity is maintained by passing an
    /// ordered header list to `build_response`.
    pub fn serialize(&self) -> Vec<u8> {
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("RTSP/1.0 {} {}", self.status_code, self.status_phrase()));
        lines.push(format!("CSeq: {}", self.cseq));

        for (key, value) in &self.headers {
            lines.push(format!("{key}: {value}"));
        }

        if !self.body.is_empty() {
            lines.push(format!("Content-Length: {}", self.body.len()));
            lines.push(String::new());
            lines.push(self.body.clone());
        } else {
            lines.push("Content-Length: 0".to_string());
            lines.push(String::new());
            lines.push(String::new());
        }

        lines.join("\r\n").into_bytes()
    }
}

/// Parse an RTSP request from raw bytes.
pub fn parse_rtsp_request(data: &[u8]) -> Result<RtspRequest, RtspParseError> {
    if data.is_empty() {
        return Err(RtspParseError::new("Empty request"));
    }

    // UTF-8 with replacement, matching Python errors="replace".
    let text = String::from_utf8_lossy(data).into_owned();

    // Find header/body separator.
    let (header_text, body_text) = if let Some(idx) = text.find("\r\n\r\n") {
        (text[..idx].to_string(), text[idx + 4..].to_string())
    } else if let Some(idx) = text.find("\n\n") {
        (text[..idx].to_string(), text[idx + 2..].to_string())
    } else {
        (text.clone(), String::new())
    };

    if header_text.len() > MAX_HEADER_SIZE {
        return Err(RtspParseError::with_code(
            "Request header exceeds maximum size",
            RTSP_REQUEST_ENTITY_TOO_LARGE,
        ));
    }
    if body_text.len() > MAX_BODY_SIZE {
        return Err(RtspParseError::with_code(
            "Request body exceeds maximum size",
            RTSP_REQUEST_ENTITY_TOO_LARGE,
        ));
    }

    let lines: Vec<&str> = if header_text.contains("\r\n") {
        header_text.split("\r\n").collect()
    } else {
        header_text.split('\n').collect()
    };
    if lines.is_empty() {
        return Err(RtspParseError::new("No request line"));
    }

    let request_line = lines[0].trim();
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(RtspParseError::new(format!(
            "Malformed request line: {request_line:?}"
        )));
    }

    let method_str = parts[0];
    let uri = parts[1].to_string();
    let version = parts[2].to_string();

    let method = RtspMethod::from_str(method_str).ok_or_else(|| {
        RtspParseError::with_code(format!("Unknown method: {method_str}"), RTSP_NOT_IMPLEMENTED)
    })?;

    if !version.starts_with("RTSP/") {
        return Err(RtspParseError::new(format!(
            "Invalid protocol version: {version}"
        )));
    }

    // Parse headers (case-preserving keys, like the Python dict).
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for line in &lines[1..] {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    // Extract CSeq (required) — check the case variants the Python checked.
    let cseq_str = headers
        .get("CSeq")
        .or_else(|| headers.get("cseq"))
        .or_else(|| headers.get("CSEQ"));
    let cseq_str = cseq_str.ok_or_else(|| RtspParseError::new("Missing required CSeq header"))?;
    let cseq: i64 = cseq_str
        .parse()
        .map_err(|_| RtspParseError::new(format!("Invalid CSeq value: {cseq_str:?}")))?;
    if cseq < 0 {
        return Err(RtspParseError::new(format!("Invalid CSeq value: {cseq}")));
    }

    // Extract Content-Length (defaults to 0 on any parse failure).
    let content_length: i64 = headers
        .get("Content-Length")
        .or_else(|| headers.get("content-length"))
        .or_else(|| headers.get("Content-length"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Trim body to content-length.
    let body = if content_length > 0 && !body_text.is_empty() {
        let n = (content_length as usize).min(body_text.len());
        body_text[..n].to_string()
    } else {
        body_text
    };

    Ok(RtspRequest {
        method,
        uri,
        version,
        cseq,
        headers,
        body,
        content_length,
    })
}

/// Build an RTSP response. `headers` preserves the order given.
pub fn build_response(
    status_code: u16,
    cseq: i64,
    headers: Option<Vec<(&str, &str)>>,
    body: &str,
) -> RtspResponse {
    let mut map = BTreeMap::new();
    if let Some(hs) = headers {
        for (k, v) in hs {
            map.insert(k.to_string(), v.to_string());
        }
    }
    RtspResponse {
        status_code,
        cseq,
        headers: map,
        body: body.to_string(),
    }
}

/// Build response to OPTIONS request (advertises sink methods, FR-RN02).
pub fn build_options_response(cseq: i64) -> RtspResponse {
    build_response(
        RTSP_OK,
        cseq,
        Some(vec![(
            "Public",
            "org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN",
        )]),
        "",
    )
}

/// Build an OPTIONS request from the sink to the source (M2).
pub fn build_options_request(cseq: i64) -> Vec<u8> {
    format!("OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nRequire: org.wfa.wfd1.0\r\n\r\n")
        .into_bytes()
}

// ─── WFD Parameter Handling ───────────────────────────────────────────────────

/// Parsed WFD video format parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfdVideoFormat {
    pub native_index: u32,
    pub preferred_display_mode: u32,
    pub profile: u32,
    pub level: u32,
    pub cea_bitmap: u32,
    pub vesa_bitmap: u32,
    pub hh_bitmap: u32,
    pub latency: u32,
    pub min_slice_size: u32,
    pub slice_enc_params: u32,
    pub frame_rate_control: u32,
    pub max_hres: String,
    pub max_vres: String,
}

impl Default for WfdVideoFormat {
    fn default() -> Self {
        Self {
            native_index: 0,
            preferred_display_mode: 0,
            profile: 0x02,          // CHP
            level: 0x10,            // Level 4.2
            cea_bitmap: 0x0001DEFF, // Standard supported resolutions
            vesa_bitmap: 0x0000_0000,
            hh_bitmap: 0x0000_0000,
            latency: 0,
            min_slice_size: 0,
            slice_enc_params: 0,
            frame_rate_control: 0,
            max_hres: "none".to_string(),
            max_vres: "none".to_string(),
        }
    }
}

impl WfdVideoFormat {
    /// Serialize to WFD response format (matches Python `to_wfd_string`).
    pub fn to_wfd_string(&self) -> String {
        format!(
            "{:02X} {:02X} {:02X} {:02X} {:08X} {:08X} {:08X} {:02X} {:04X} {:04X} {:02X} {} {}",
            self.native_index,
            self.preferred_display_mode,
            self.profile,
            self.level,
            self.cea_bitmap,
            self.vesa_bitmap,
            self.hh_bitmap,
            self.latency,
            self.min_slice_size,
            self.slice_enc_params,
            self.frame_rate_control,
            self.max_hres,
            self.max_vres,
        )
    }
}

/// Parsed WFD audio codec parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfdAudioCodec {
    pub codec: String,
    pub modes_bitmap: u32,
    pub latency: u32,
}

impl Default for WfdAudioCodec {
    fn default() -> Self {
        Self {
            codec: "AAC".to_string(),
            modes_bitmap: 0x0000_0007,
            latency: 0,
        }
    }
}

impl WfdAudioCodec {
    pub fn to_wfd_string(&self) -> String {
        format!("{} {:08X} {:02X}", self.codec, self.modes_bitmap, self.latency)
    }
}

/// Parsed WFD parameters from a SET_PARAMETER request body.
#[derive(Debug, Clone, Default)]
pub struct WfdParameters {
    pub video_formats: Option<WfdVideoFormat>,
    pub audio_codecs: Option<WfdAudioCodec>,
    pub presentation_url: String,
    pub client_rtp_ports: String,
    pub content_protection: String,
    pub rtp_port: i64,
    pub video_codec: String,
    pub audio_codec: String,
    pub resolution: (u32, u32),
}

/// Parse WFD parameters from an RTSP SET_PARAMETER body.
pub fn parse_wfd_parameters(body: &str) -> WfdParameters {
    let mut params = WfdParameters {
        content_protection: String::new(),
        ..Default::default()
    };

    for raw in body.trim().split('\n') {
        let line = raw.trim();
        if line.is_empty() || !line.contains(':') {
            continue;
        }
        let (key_raw, value_raw) = line.split_once(':').unwrap();
        let key = key_raw.trim().to_ascii_lowercase();
        let value = value_raw.trim();

        match key.as_str() {
            "wfd_video_formats" => {
                let fmt = parse_video_formats(value);
                params.resolution = resolution_from_cea_bitmap(fmt.cea_bitmap);
                params.video_formats = Some(fmt);
                params.video_codec = "H264".to_string();
            }
            "wfd_audio_codecs" => {
                let codec = parse_audio_codecs(value);
                params.audio_codec = codec.codec.clone();
                params.audio_codecs = Some(codec);
            }
            "wfd_presentation_url" => params.presentation_url = value.to_string(),
            "wfd_client_rtp_ports" => {
                params.client_rtp_ports = value.to_string();
                let parts: Vec<&str> = value.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(p) = parts[1].parse::<i64>() {
                        params.rtp_port = p;
                    }
                }
            }
            "wfd_content_protection" => params.content_protection = value.to_string(),
            _ => {}
        }
    }

    params
}

fn parse_hex_field(parts: &[&str], idx: usize, current: u32) -> u32 {
    parts
        .get(idx)
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .unwrap_or(current)
}

fn parse_video_formats(value: &str) -> WfdVideoFormat {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let mut fmt = WfdVideoFormat::default();

    fmt.native_index = parse_hex_field(&parts, 0, fmt.native_index);
    fmt.preferred_display_mode = parse_hex_field(&parts, 1, fmt.preferred_display_mode);
    fmt.profile = parse_hex_field(&parts, 2, fmt.profile);
    fmt.level = parse_hex_field(&parts, 3, fmt.level);
    fmt.cea_bitmap = parse_hex_field(&parts, 4, fmt.cea_bitmap);
    fmt.vesa_bitmap = parse_hex_field(&parts, 5, fmt.vesa_bitmap);
    fmt.hh_bitmap = parse_hex_field(&parts, 6, fmt.hh_bitmap);
    fmt.latency = parse_hex_field(&parts, 7, fmt.latency);
    fmt.min_slice_size = parse_hex_field(&parts, 8, fmt.min_slice_size);
    fmt.slice_enc_params = parse_hex_field(&parts, 9, fmt.slice_enc_params);
    fmt.frame_rate_control = parse_hex_field(&parts, 10, fmt.frame_rate_control);
    if let Some(v) = parts.get(11) {
        fmt.max_hres = v.to_string();
    }
    if let Some(v) = parts.get(12) {
        fmt.max_vres = v.to_string();
    }
    fmt
}

fn parse_audio_codecs(value: &str) -> WfdAudioCodec {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let mut codec = WfdAudioCodec::default();
    if let Some(c) = parts.first() {
        codec.codec = c.to_string();
    }
    codec.modes_bitmap = parse_hex_field(&parts, 1, codec.modes_bitmap);
    codec.latency = parse_hex_field(&parts, 2, codec.latency);
    codec
}

/// Determine the highest resolution from a CEA bitmap.
fn resolution_from_cea_bitmap(bitmap: u32) -> (u32, u32) {
    // Bit position → resolution, highest first.
    const CEA: &[(u32, (u32, u32))] = &[
        (8, (1920, 1080)),
        (7, (1920, 1080)),
        (6, (1280, 720)),
        (5, (1280, 720)),
        (4, (720, 576)),
        (3, (720, 576)),
        (2, (720, 480)),
        (1, (720, 480)),
        (0, (640, 480)),
    ];
    for (bit, res) in CEA {
        if bitmap & (1 << bit) != 0 {
            return *res;
        }
    }
    (1920, 1080) // Default
}

/// Build the WFD capability response body for GET_PARAMETER (sink reply to M3).
pub fn build_capability_response_body(
    rtp_port: i64,
    video_formats: Option<&WfdVideoFormat>,
    audio_codecs: Option<&WfdAudioCodec>,
) -> String {
    let default_v = WfdVideoFormat::default();
    let default_a = WfdAudioCodec::default();
    let v = video_formats.unwrap_or(&default_v);
    let a = audio_codecs.unwrap_or(&default_a);

    let lines = [
        format!("wfd_video_formats: {}", v.to_wfd_string()),
        format!("wfd_audio_codecs: {}", a.to_wfd_string()),
        format!("wfd_client_rtp_ports: RTP/AVP/UDP;unicast {rtp_port} 0 mode=play"),
        "wfd_content_protection: none".to_string(),
        "wfd_coupled_sink: none".to_string(),
    ];
    lines.join("\r\n")
}

/// Validate that raw request data does not exceed size limits.
pub fn validate_request_size(data: &[u8]) -> Result<(), RtspParseError> {
    if data.len() > MAX_HEADER_SIZE + MAX_BODY_SIZE {
        return Err(RtspParseError::with_code(
            "Request exceeds maximum allowed size",
            RTSP_REQUEST_ENTITY_TOO_LARGE,
        ));
    }

    // Header size check.
    if let Some(sep) = find_subslice(data, b"\r\n\r\n") {
        let header_size = sep;
        let body_size = data.len() - sep - 4;
        if header_size > MAX_HEADER_SIZE {
            return Err(RtspParseError::with_code(
                format!("Request headers exceed {MAX_HEADER_SIZE} bytes"),
                RTSP_REQUEST_ENTITY_TOO_LARGE,
            ));
        }
        if body_size > MAX_BODY_SIZE {
            return Err(RtspParseError::with_code(
                format!("Request body exceeds {MAX_BODY_SIZE} bytes"),
                RTSP_REQUEST_ENTITY_TOO_LARGE,
            ));
        }
    } else if data.len() > MAX_HEADER_SIZE {
        return Err(RtspParseError::with_code(
            format!("Request headers exceed {MAX_HEADER_SIZE} bytes (no body separator)"),
            RTSP_REQUEST_ENTITY_TOO_LARGE,
        ));
    }

    // Content-Length declared-size check.
    let text = String::from_utf8_lossy(data);
    for line in text.split("\r\n") {
        if line.to_ascii_lowercase().starts_with("content-length:") {
            let cl = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
            match cl.parse::<i64>() {
                Ok(n) if n > MAX_BODY_SIZE as i64 => {
                    return Err(RtspParseError::with_code(
                        format!("Content-Length {n} exceeds {MAX_BODY_SIZE} bytes"),
                        RTSP_REQUEST_ENTITY_TOO_LARGE,
                    ));
                }
                Ok(n) if n < 0 => {
                    return Err(RtspParseError::with_code(
                        "Negative Content-Length",
                        RTSP_BAD_REQUEST,
                    ));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(RtspParseError::with_code(
                        "Invalid Content-Length value",
                        RTSP_BAD_REQUEST,
                    ));
                }
            }
            break;
        }
    }
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_response_serializes_expected_wire_bytes() {
        let resp = build_options_response(1);
        let wire = String::from_utf8(resp.serialize()).unwrap();
        assert!(wire.starts_with("RTSP/1.0 200 OK\r\nCSeq: 1\r\n"));
        assert!(wire.contains(
            "Public: org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN"
        ));
        assert!(wire.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn video_format_default_wfd_string() {
        // Byte-exact against the Python default WFDVideoFormat.to_wfd_string().
        let s = WfdVideoFormat::default().to_wfd_string();
        assert_eq!(
            s,
            "00 00 02 10 0001DEFF 00000000 00000000 00 0000 0000 00 none none"
        );
    }

    #[test]
    fn audio_codec_default_wfd_string() {
        assert_eq!(WfdAudioCodec::default().to_wfd_string(), "AAC 00000007 00");
    }

    #[test]
    fn parse_request_extracts_cseq_and_method() {
        let raw = b"OPTIONS * RTSP/1.0\r\nCSeq: 5\r\nRequire: org.wfa.wfd1.0\r\n\r\n";
        let req = parse_rtsp_request(raw).unwrap();
        assert_eq!(req.method, RtspMethod::Options);
        assert_eq!(req.cseq, 5);
        assert_eq!(req.uri, "*");
    }

    #[test]
    fn missing_cseq_is_error() {
        let raw = b"OPTIONS * RTSP/1.0\r\nRequire: org.wfa.wfd1.0\r\n\r\n";
        let err = parse_rtsp_request(raw).unwrap_err();
        assert_eq!(err.status_code, RTSP_BAD_REQUEST);
    }

    #[test]
    fn unknown_method_is_501() {
        let raw = b"FROB * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let err = parse_rtsp_request(raw).unwrap_err();
        assert_eq!(err.status_code, RTSP_NOT_IMPLEMENTED);
    }

    #[test]
    fn cea_bitmap_resolution() {
        assert_eq!(resolution_from_cea_bitmap(0x0000_0100), (1920, 1080)); // bit 8
        assert_eq!(resolution_from_cea_bitmap(0x0000_0040), (1280, 720)); // bit 6
        assert_eq!(resolution_from_cea_bitmap(0x0000_0001), (640, 480)); // bit 0
        assert_eq!(resolution_from_cea_bitmap(0), (1920, 1080)); // default
    }

    #[test]
    fn capability_body_has_expected_lines() {
        let body = build_capability_response_body(1028, None, None);
        assert!(body.contains("wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play"));
        assert!(body.contains("wfd_content_protection: none"));
        assert!(body.contains("wfd_coupled_sink: none"));
    }

    #[test]
    fn parse_wfd_params_reads_rtp_port_and_codec() {
        let body = "wfd_audio_codecs: AAC 00000001 00\n\
                    wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play\n\
                    wfd_video_formats: 00 00 02 10 0001FEFF 3FFFFFFF 00000FFF 00 0000 0000 00 none none";
        let p = parse_wfd_parameters(body);
        assert_eq!(p.rtp_port, 1028);
        assert_eq!(p.audio_codec, "AAC");
        assert_eq!(p.video_codec, "H264");
    }

    #[test]
    fn oversize_content_length_rejected() {
        let raw = format!(
            "SET_PARAMETER * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_SIZE + 1
        );
        let err = validate_request_size(raw.as_bytes()).unwrap_err();
        assert_eq!(err.status_code, RTSP_REQUEST_ENTITY_TOO_LARGE);
    }
}
