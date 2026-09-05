//! Miracast Receiver — RTSP client and GStreamer pipeline management.
//!
//! In Wi-Fi Display, the Sink (us) connects TO the Source's RTSP server on
//! port 7236, so the Sink is the RTSP client. Faithful port of
//! `src/miracast_server/receiver.py`, driving GStreamer in-process via
//! gstreamer-rs to preserve the dynamic pipeline, pad-probe byte stats, and
//! bus-driven HW→SW decoder fallback.
//!
//! RTSP message flow (from lazycast / Wi-Fi Display spec):
//!   Sink connects to Source:7236
//!   M1: Source → OPTIONS → Sink 200 OK
//!   M2: Sink → OPTIONS → Source 200 OK
//!   M3: Source → GET_PARAMETER (query caps) → Sink replies with WFD params
//!   M4: Source → SET_PARAMETER (chosen params) → Sink 200 OK
//!   M5: Source → SET_PARAMETER (trigger SETUP) → Sink 200 OK
//!   M6: Sink → SETUP …/streamid=0 → Source replies with Session
//!   M7: Sink → PLAY …/streamid=0 → Source 200 OK, starts RTP

use crate::events::{Event, EventSender, StreamStats};
use crate::models::{IncomingConnection, ReceiverStats, SourceInfo};
use crate::sync_ext::LockExt;

use gstreamer as gst;
use gstreamer::prelude::*;

use chrono::Local;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Codec whitelist for pipeline construction.
const ALLOWED_VIDEO_CODECS: &[&str] = &["H264"];
const ALLOWED_AUDIO_CODECS: &[&str] = &["AAC"];

// Stream monitoring constants.
const RTP_TIMEOUT: Duration = Duration::from_secs(15);
const STATS_INTERVAL: Duration = Duration::from_secs(1);
const FRAME_DROP_WARNING_THRESHOLD: f64 = 0.05; // 5%
const FRAME_DROP_WINDOW: Duration = Duration::from_secs(10);

// Queue bounds. QUEUE_MAX_TIME is deliberately SMALL: this is a LIVE Miracast
// feed, so a large time buffer just becomes accumulated latency (lag). 200ms is
// enough to absorb Wi-Fi jitter without a visible delay.
const QUEUE_MAX_BUFFERS: u32 = 200;
const QUEUE_MAX_BYTES: u32 = 10_485_760; // 10 MB
const QUEUE_MAX_TIME: u64 = 200_000_000; // 200 ms in ns

// RTSP connection constants.
const RTSP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RTSP_RECV_TIMEOUT: Duration = Duration::from_secs(30);
const RTSP_BUFFER_SIZE: usize = 16384;

fn validate_port(port: i64) -> Result<(), String> {
    if !(1024..=65535).contains(&port) {
        return Err(format!(
            "Port must be integer in range 1024-65535, got {port}"
        ));
    }
    Ok(())
}
fn validate_video_codec(codec: &str) -> Result<(), String> {
    if !ALLOWED_VIDEO_CODECS.contains(&codec) {
        return Err(format!(
            "Video codec '{codec}' not in whitelist: {ALLOWED_VIDEO_CODECS:?}"
        ));
    }
    Ok(())
}
fn validate_audio_codec(codec: &str) -> Result<(), String> {
    if !ALLOWED_AUDIO_CODECS.contains(&codec) {
        return Err(format!(
            "Audio codec '{codec}' not in whitelist: {ALLOWED_AUDIO_CODECS:?}"
        ));
    }
    Ok(())
}

/// Ensure GStreamer is initialized exactly once (Python did `Gst.init(None)`).
pub fn gst_init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = gst::init();
    });
}

/// Constructs GStreamer pipelines for Miracast stream reception.
pub struct PipelineBuilder {
    headless: bool,
}

impl PipelineBuilder {
    pub fn new(headless: bool) -> Self {
        Self { headless }
    }

    /// Build the receive pipeline:
    ///   udpsrc → rtpmp2tdepay → tsdemux → h264parse → decoder → videoconvert → sink
    ///   (audio): aacparse → avdec_aac → audioconvert → pulsesink
    pub fn build_pipeline(
        &self,
        rtp_port: i64,
        video_codec: &str,
        audio_codec: &str,
        audio_enabled: bool,
        use_hw_decode: bool,
    ) -> Result<gst::Pipeline, String> {
        validate_port(rtp_port)?;
        validate_video_codec(video_codec)?;
        let mut audio_enabled = audio_enabled;
        if audio_enabled {
            validate_audio_codec(audio_codec)?;
        }

        let pipeline = gst::Pipeline::with_name("miracast-receive");

        // ── Source and demux ──
        let udpsrc = make("udpsrc", "udpsrc")?;
        udpsrc.set_property("port", rtp_port as i32);
        udpsrc.set_property("buffer-size", 2 * 1024 * 1024_i32);
        let caps = gst::Caps::from_str(
            "application/x-rtp,media=video,clock-rate=90000,encoding-name=MP2T",
        )
        .map_err(|e| format!("bad caps: {e}"))?;
        udpsrc.set_property("caps", &caps);

        // RTP jitter buffer: reorders out-of-order UDP packets and paces them
        // out. Tuned for LIVE low latency — the default would buffer and can
        // grow, adding lag. A short bounded latency (80ms) absorbs Wi-Fi jitter;
        // drop-on-latency skips a packet that arrives too late instead of
        // stalling the stream (which is what accumulates delay on a P2P link);
        // do-lost lets the decoder conceal a gap rather than freeze.
        let jitterbuffer = make("rtpjitterbuffer", "jitterbuffer")?;
        set_if_has(&jitterbuffer, "latency", 80u32); // ms
        set_if_has(&jitterbuffer, "drop-on-latency", true);
        set_if_has(&jitterbuffer, "do-lost", true);

        let rtpdepay = make("rtpmp2tdepay", "rtpdepay")?;
        let tsdemux = make("tsdemux", "demux")?;

        // ── Video branch ──
        let video_queue = self.make_queue("video_queue")?;
        let h264parse = make("h264parse", "h264parse")?;
        let decoder = self.make_decoder(use_hw_decode)?;
        let videoconvert = make("videoconvert", "videoconvert")?;
        let videosink = self.make_video_sink()?;

        let mut elems = vec![
            udpsrc.clone(),
            jitterbuffer.clone(),
            rtpdepay.clone(),
            tsdemux.clone(),
            video_queue.clone(),
            h264parse.clone(),
            decoder.clone(),
            videoconvert.clone(),
            videosink.clone(),
        ];
        for e in &elems {
            pipeline.add(e).map_err(|e| format!("pipeline.add: {e}"))?;
        }

        udpsrc
            .link(&jitterbuffer)
            .map_err(|e| format!("link udpsrc→jitterbuffer: {e}"))?;
        jitterbuffer
            .link(&rtpdepay)
            .map_err(|e| format!("link jitterbuffer→rtpdepay: {e}"))?;
        rtpdepay
            .link(&tsdemux)
            .map_err(|e| format!("link rtpdepay→tsdemux: {e}"))?;
        video_queue
            .link(&h264parse)
            .map_err(|e| format!("link vq→h264parse: {e}"))?;
        h264parse
            .link(&decoder)
            .map_err(|e| format!("link h264parse→dec: {e}"))?;
        decoder
            .link(&videoconvert)
            .map_err(|e| format!("link dec→convert: {e}"))?;
        videoconvert
            .link(&videosink)
            .map_err(|e| format!("link convert→sink: {e}"))?;

        // ── Audio branch (optional) ──
        let audio_queue_opt = if audio_enabled {
            let audio_queue = self.make_queue("audio_queue")?;
            let aacparse = gst::ElementFactory::make("aacparse")
                .name("aacparse")
                .build()
                .ok();
            let audiodec = gst::ElementFactory::make("avdec_aac")
                .name("audiodec")
                .build()
                .ok();
            let audioconvert = gst::ElementFactory::make("audioconvert")
                .name("audioconvert")
                .build()
                .ok();
            let mut audiosink = self.make_audio_sink();

            if aacparse.is_none()
                || audiodec.is_none()
                || audioconvert.is_none()
                || audiosink.is_none()
            {
                // Fallback: autoaudiosink.
                audiosink = gst::ElementFactory::make("autoaudiosink")
                    .name("audiosink")
                    .build()
                    .ok();
                if audiosink.is_none() {
                    log::warn!("No audio sink available, disabling audio");
                    audio_enabled = false;
                }
            }

            if audio_enabled {
                let aacparse = aacparse.unwrap();
                let audiodec = audiodec.unwrap();
                let audioconvert = audioconvert.unwrap();
                let audiosink = audiosink.unwrap();
                for e in [
                    &audio_queue,
                    &aacparse,
                    &audiodec,
                    &audioconvert,
                    &audiosink,
                ] {
                    pipeline
                        .add(e)
                        .map_err(|e| format!("pipeline.add audio: {e}"))?;
                    elems.push(e.clone());
                }
                audio_queue
                    .link(&aacparse)
                    .map_err(|e| format!("link aq→aacparse: {e}"))?;
                aacparse
                    .link(&audiodec)
                    .map_err(|e| format!("link aacparse→dec: {e}"))?;
                audiodec
                    .link(&audioconvert)
                    .map_err(|e| format!("link adec→conv: {e}"))?;
                audioconvert
                    .link(&audiosink)
                    .map_err(|e| format!("link aconv→sink: {e}"))?;
                Some(audio_queue)
            } else {
                None
            }
        } else {
            None
        };

        // ── Dynamic pad linking for tsdemux ──
        let vq = video_queue.clone();
        let aq = audio_queue_opt.clone();
        let audio_on = audio_enabled;
        tsdemux.connect_pad_added(move |_demux, pad| {
            let caps_str = pad
                .current_caps()
                .map(|c| c.to_string())
                .unwrap_or_default();
            let lc = caps_str.to_lowercase();
            log::debug!("tsdemux pad added: {} caps={caps_str}", pad.name());

            if caps_str.contains("video") || lc.contains("h264") {
                if let Some(sink_pad) = vq.static_pad("sink") {
                    if !sink_pad.is_linked() {
                        let _ = pad.link(&sink_pad);
                        log::debug!("Linked video pad");
                    }
                }
            } else if audio_on && (caps_str.contains("audio") || lc.contains("aac")) {
                if let Some(ref aq) = aq {
                    if let Some(sink_pad) = aq.static_pad("sink") {
                        if !sink_pad.is_linked() {
                            let _ = pad.link(&sink_pad);
                            log::debug!("Linked audio pad");
                        }
                    }
                }
            }
        });

        Ok(pipeline)
    }

    fn make_queue(&self, name: &str) -> Result<gst::Element, String> {
        let q = make("queue", name)?;
        q.set_property("max-size-buffers", QUEUE_MAX_BUFFERS);
        q.set_property("max-size-bytes", QUEUE_MAX_BYTES);
        q.set_property("max-size-time", QUEUE_MAX_TIME);
        // NOT leaky: this queue feeds h264parse→decoder, so it carries COMPRESSED
        // frames. Dropping a compressed frame loses a reference/NAL unit and
        // corrupts decode downstream (blocky/green artifacts) — worse than a
        // brief stall. Frame-dropping, if needed, belongs at the SINK via QoS,
        // which we deliberately disable to preserve quality.
        Ok(q)
    }

    /// Create a video decoder, attempting hardware acceleration first.
    fn make_decoder(&self, use_hw: bool) -> Result<gst::Element, String> {
        if use_hw {
            for hw in ["vaapidecodebin", "nvh264dec"] {
                if gst::ElementFactory::find(hw).is_some() {
                    if let Ok(dec) = gst::ElementFactory::make(hw).name("videodec").build() {
                        log::info!("Using hardware decoder: {hw}");
                        return Ok(dec);
                    }
                }
            }
        }
        let dec = gst::ElementFactory::make("avdec_h264")
            .name("videodec")
            .build()
            .map_err(|_| "Failed to create video decoder (avdec_h264)".to_string())?;
        // Multi-threaded decode so 1080p keeps up in software (single-threaded
        // avdec_h264 falling behind real time shows as stutter/poor quality).
        // max-threads=0 = auto (all cores); prefer low-latency slice threading.
        set_if_has(&dec, "max-threads", 0i32);
        set_if_has(&dec, "std-compliance", 0i32);
        log::info!("Using software decoder: avdec_h264 (multi-threaded)");
        Ok(dec)
    }

    /// Build the audio sink (pulsesink). When running as ROOT via `sudo`, root
    /// has no PulseAudio session of its own, so a bare pulsesink fails to
    /// connect ("Connection refused"). But the invoking user (`$SUDO_USER`) does
    /// have a PA daemon at `/run/user/<uid>/pulse/native` — point pulsesink's
    /// `server` there so audio plays under sudo too. When not root, leave
    /// `server` unset (connects to the caller's own session normally).
    fn make_audio_sink(&self) -> Option<gst::Element> {
        let sink = gst::ElementFactory::make("pulsesink")
            .name("audiosink")
            .build()
            .ok()?;
        // `SUDO_USER` is set exactly when we were launched via sudo, i.e. we are
        // root but a real user session exists. Route pulsesink to that user's PA
        // socket so audio plays under sudo. When not set, leave `server` unset
        // (connects to the caller's own session normally).
        if let Ok(sudo_user) = std::env::var("SUDO_USER") {
            if let Some(uid) = uid_of_user(&sudo_user) {
                let server = format!("/run/user/{uid}/pulse/native");
                if std::path::Path::new(&server).exists() {
                    sink.set_property("server", &server);
                    // PA may require the user's cookie to accept a cross-uid
                    // (root→user) connection; point PULSE_COOKIE at it so the
                    // connection authenticates. Best-effort.
                    let cookie = format!("/run/user/{uid}/pulse/cookie");
                    if std::path::Path::new(&cookie).exists() {
                        std::env::set_var("PULSE_COOKIE", &cookie);
                    }
                    log::info!("Audio: routing pulsesink to {sudo_user}'s PulseAudio ({server})");
                } else {
                    log::warn!(
                        "Audio: {sudo_user}'s PulseAudio socket {server} not found; \
                         audio may fail under sudo (run non-root for reliable audio)"
                    );
                }
            }
        }
        Some(sink)
    }

    /// Create the video sink element based on mode.
    fn make_video_sink(&self) -> Result<gst::Element, String> {
        if self.headless {
            let sink = make("fakesink", "videosink")?;
            sink.set_property("sync", true);
            return Ok(sink);
        }
        if let Ok(sink) = gst::ElementFactory::make("gtk4paintablesink")
            .name("videosink")
            .build()
        {
            // Live low-latency display: render frames as they arrive.
            // sync=false — do NOT hold frames to their presentation timestamps;
            //   for a live Miracast feed, PTS-syncing turns initial buffering
            //   into permanent lag that only grows. Display on arrival instead.
            // qos=false — don't let the sink ask upstream to DROP frames to
            //   catch up (that shows as stutter/smearing).
            set_if_has(&sink, "sync", false);
            set_if_has(&sink, "qos", false);
            log::info!("Using gtk4paintablesink for video output (live, sync off)");
            return Ok(sink);
        }
        let sink = gst::ElementFactory::make("autovideosink")
            .name("videosink")
            .build()
            .map_err(|_| "Failed to create video sink element".to_string())?;
        // autovideosink proxies to its child sink; its own sync/qos forward.
        set_if_has(&sink, "sync", false);
        set_if_has(&sink, "qos", false);
        log::info!("Using autovideosink for video output (live, sync off)");
        Ok(sink)
    }
}

fn make(factory: &str, name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|_| format!("Failed to create {factory} element"))
}

/// Set a property only if the element actually exposes it. GStreamer element
/// properties vary by version/plugin build, so a hard `set_property` on a
/// missing property — or one whose value type differs (e.g. an enum vs a plain
/// gint) — panics. This sets ONLY when the property exists AND the value type
/// matches, otherwise it no-ops (with a debug note), keeping tuning best-effort
/// and never taking down the pipeline thread over a version-specific property.
fn set_if_has(el: &gst::Element, name: &str, value: impl Into<gst::glib::Value>) {
    let value = value.into();
    match el.find_property(name) {
        Some(pspec) if pspec.value_type() == value.type_() => {
            el.set_property_from_value(name, &value);
        }
        Some(pspec) => {
            log::debug!(
                "skip set '{name}': type mismatch (property {}, value {})",
                pspec.value_type(),
                value.type_()
            );
        }
        None => {}
    }
}

/// Resolve a username to its numeric uid by parsing `/etc/passwd`
/// (dependency-free). Locates the invoking user's PulseAudio socket
/// (`/run/user/<uid>/pulse/native`) when running under sudo.
fn uid_of_user(user: &str) -> Option<u32> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut f = line.split(':');
        if f.next() == Some(user) {
            return f.nth(1).and_then(|uid| uid.parse::<u32>().ok());
        }
    }
    None
}

/// Fast, up-front probe: can ANY audio sink actually open on this host/run
/// context? pulsesink connects to PulseAudio at the READY transition, which
/// fails ("Connection refused") when there is no user PA session (e.g. under
/// sudo). Test it cheaply — bring a standalone sink to READY with a short wait —
/// so the pipeline is built audio-on ONLY when audio can work, avoiding a failed
/// PLAYING attempt that would delay the RTSP M7 response.
pub(crate) fn audio_sink_available() -> bool {
    // Under sudo, route the probe's pulsesink at the invoking user's PA socket
    // (same as make_audio_sink) so the probe reflects the sink the pipeline
    // will actually use, not a session-less pulsesink that always fails.
    let sudo_server = std::env::var("SUDO_USER")
        .ok()
        .and_then(|u| uid_of_user(&u))
        .map(|uid| format!("/run/user/{uid}/pulse/native"))
        .filter(|p| std::path::Path::new(p).exists());

    for factory in ["pulsesink", "autoaudiosink"] {
        let Ok(sink) = gst::ElementFactory::make(factory).build() else {
            continue;
        };
        if factory == "pulsesink" {
            if let Some(ref server) = sudo_server {
                sink.set_property("server", server);
            }
        }
        if sink.set_state(gst::State::Ready).is_ok() {
            let settled = sink.state(gst::ClockTime::from_seconds(2)).0.is_ok();
            let _ = sink.set_state(gst::State::Null);
            if settled {
                log::debug!("Audio sink available: {factory}");
                return true;
            }
        } else {
            let _ = sink.set_state(gst::State::Null);
        }
    }
    false
}

/// Shared, thread-safe receiver state (counters updated from probe + threads).
struct RxState {
    running: AtomicBool,
    data_received: AtomicU64,
    frames_decoded: AtomicI64,
    frames_dropped: AtomicI64,
    // Monotonic nanoseconds since an epoch instant, for last RTP arrival.
    last_rtp_ns: AtomicU64,
    errors: AtomicI64,
    peak_bitrate_milli: AtomicU64, // peak bitrate * 1000, integer-encoded
    current_bitrate_milli: AtomicU64,
    resolution: Mutex<(u32, u32)>,
    use_hw_decode: AtomicBool,
}

impl RxState {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            data_received: AtomicU64::new(0),
            frames_decoded: AtomicI64::new(0),
            frames_dropped: AtomicI64::new(0),
            last_rtp_ns: AtomicU64::new(0),
            errors: AtomicI64::new(0),
            peak_bitrate_milli: AtomicU64::new(0),
            current_bitrate_milli: AtomicU64::new(0),
            resolution: Mutex::new((0, 0)),
            use_hw_decode: AtomicBool::new(true),
        }
    }
}

/// Manages RTSP client session + GStreamer pipeline for receiving streams.
pub struct MiracastReceiver {
    rtsp_port: u16,
    rtp_port: Arc<AtomicI64>,
    headless: bool,
    audio_enabled: bool,
    events: EventSender,
    max_resolution: Arc<Mutex<(u32, u32)>>,

    state: Arc<RxState>,
    pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    epoch: Instant,

    connection: Option<IncomingConnection>,
    source_info: Arc<Mutex<Option<SourceInfo>>>,
    start_time: Arc<Mutex<Option<chrono::DateTime<Local>>>>,
    video_codec: Arc<Mutex<String>>,
    audio_codec: Arc<Mutex<String>>,

    rtsp_thread: Option<std::thread::JoinHandle<()>>,
    stats_thread: Option<std::thread::JoinHandle<()>>,
}

impl MiracastReceiver {
    pub fn new(
        rtsp_port: u16,
        rtp_port: i64,
        headless: bool,
        audio_enabled: bool,
        events: EventSender,
        max_resolution: Arc<Mutex<(u32, u32)>>,
    ) -> Self {
        gst_init();
        Self {
            rtsp_port,
            rtp_port: Arc::new(AtomicI64::new(rtp_port)),
            headless,
            audio_enabled,
            events,
            max_resolution,
            state: Arc::new(RxState::new()),
            pipeline: Arc::new(Mutex::new(None)),
            epoch: Instant::now(),
            connection: None,
            source_info: Arc::new(Mutex::new(None)),
            start_time: Arc::new(Mutex::new(None)),
            video_codec: Arc::new(Mutex::new("H264".to_string())),
            audio_codec: Arc::new(Mutex::new("AAC".to_string())),
            rtsp_thread: None,
            stats_thread: None,
        }
    }

    pub fn is_receiving(&self) -> bool {
        self.state.running.load(Ordering::SeqCst)
    }

    pub fn source_info(&self) -> Option<SourceInfo> {
        self.source_info.lock_safe().clone()
    }

    /// The pipeline handle (for GUI binding to the paintable sink).
    pub fn pipeline(&self) -> Option<gst::Pipeline> {
        self.pipeline.lock_safe().clone()
    }

    /// Start the RTSP client session with the connected source.
    pub fn start_receiving(&mut self, connection: IncomingConnection) {
        if self.state.running.load(Ordering::SeqCst) {
            log::warn!("start_receiving called while already receiving");
            return;
        }
        self.connection = Some(connection.clone());
        self.state.running.store(true, Ordering::SeqCst);
        *self.start_time.lock_safe() = Some(Local::now());
        *self.source_info.lock_safe() = Some(SourceInfo {
            name: connection.peer_name.clone(),
            address: connection.peer_address.clone(),
            model: String::new(),
            ..Default::default()
        });

        log::info!(
            "Starting RTSP client session — connecting to source {}:{}",
            connection.peer_ip,
            self.rtsp_port
        );

        let ctx = SessionCtx {
            source_ip: connection.peer_ip.clone(),
            rtsp_port: self.rtsp_port,
            rtp_port: Arc::clone(&self.rtp_port),
            headless: self.headless,
            audio_enabled: self.audio_enabled,
            events: self.events.clone(),
            max_resolution: Arc::clone(&self.max_resolution),
            state: Arc::clone(&self.state),
            pipeline: Arc::clone(&self.pipeline),
            epoch: self.epoch,
            source_info: Arc::clone(&self.source_info),
            start_time: Arc::clone(&self.start_time),
            video_codec: Arc::clone(&self.video_codec),
            audio_codec: Arc::clone(&self.audio_codec),
        };

        self.rtsp_thread = Some(
            std::thread::Builder::new()
                .name("rtsp-client".to_string())
                .spawn(move || ctx.run())
                .expect("spawn rtsp-client"),
        );
    }

    /// Stop receiving and clean up. Returns session statistics.
    pub fn stop_receiving(&mut self) -> ReceiverStats {
        self.state.running.store(false, Ordering::SeqCst);

        // Stop pipeline.
        if let Some(p) = self.pipeline.lock_safe().take() {
            let _ = p.set_state(gst::State::Null);
        }

        // Join threads (5s budget each, like Python join(timeout=5)).
        if let Some(h) = self.rtsp_thread.take() {
            join_with_timeout(h, Duration::from_secs(5), "RTSP thread");
        }
        if let Some(h) = self.stats_thread.take() {
            join_with_timeout(h, Duration::from_secs(5), "stats thread");
        }

        let stats = build_stats(&self.state, &self.start_time, &self.video_codec);
        log::info!("Receiving stopped. Duration: {}s", stats.duration);
        stats
    }
}

fn join_with_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration, label: &str) {
    let start = Instant::now();
    while !handle.is_finished() && start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(50));
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        log::warn!("{label} did not stop within {}s", timeout.as_secs());
    }
}

fn build_stats(
    state: &Arc<RxState>,
    start_time: &Arc<Mutex<Option<chrono::DateTime<Local>>>>,
    video_codec: &Arc<Mutex<String>>,
) -> ReceiverStats {
    let end_time = Local::now();
    let start = *start_time.lock_safe();
    let duration = start
        .map(|s| (end_time - s).num_seconds().max(0))
        .unwrap_or(0);
    ReceiverStats {
        start_time: start.unwrap_or(end_time),
        end_time: Some(end_time),
        duration,
        data_received: state.data_received.load(Ordering::SeqCst) as i64,
        average_bitrate: state.current_bitrate_milli.load(Ordering::SeqCst) as f64 / 1000.0,
        peak_bitrate: state.peak_bitrate_milli.load(Ordering::SeqCst) as f64 / 1000.0,
        frames_decoded: state.frames_decoded.load(Ordering::SeqCst),
        frames_dropped: state.frames_dropped.load(Ordering::SeqCst),
        errors: state.errors.load(Ordering::SeqCst),
        resolution: *state.resolution.lock_safe(),
        codec: video_codec.lock_safe().clone(),
    }
}

/// Per-session context passed to the RTSP client thread.
struct SessionCtx {
    source_ip: String,
    rtsp_port: u16,
    rtp_port: Arc<AtomicI64>,
    headless: bool,
    audio_enabled: bool,
    events: EventSender,
    max_resolution: Arc<Mutex<(u32, u32)>>,
    state: Arc<RxState>,
    pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    epoch: Instant,
    #[allow(dead_code)] // kept for structural fidelity with the Python receiver
    source_info: Arc<Mutex<Option<SourceInfo>>>,
    start_time: Arc<Mutex<Option<chrono::DateTime<Local>>>>,
    video_codec: Arc<Mutex<String>>,
    audio_codec: Arc<Mutex<String>>,
}

impl SessionCtx {
    fn run(self) {
        if let Err(msg) = self.run_inner() {
            if self.state.running.load(Ordering::SeqCst) {
                log::error!("{msg}");
                let _ = self.events.send(Event::StreamError(msg));
            }
        }
    }

    fn run_inner(&self) -> Result<(), String> {
        let mut cseq: i64 = 100;
        let mut sock = self
            .connect_to_source()
            .ok_or_else(|| "RTSP connect aborted".to_string())?;
        log::info!(
            "RTSP connected to source {}:{}",
            self.source_ip,
            self.rtsp_port
        );

        // M1: Source OPTIONS → we reply.
        let data = self
            .recv_message(&mut sock)
            .ok_or("No M1 received from source")?;
        log::info!("M1 received");
        let m1_cseq = parse_cseq(&data);
        let m1 = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {m1_cseq}\r\nPublic: org.wfa.wfd1.0, SET_PARAMETER, GET_PARAMETER\r\n\r\n"
        );
        sock.write_all(m1.as_bytes()).map_err(oserr)?;

        // M2: We send OPTIONS.
        cseq += 1;
        let m2 = format!("OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nRequire: org.wfa.wfd1.0\r\n\r\n");
        sock.write_all(m2.as_bytes()).map_err(oserr)?;
        self.recv_message(&mut sock)
            .ok_or("No M2 response from source")?;

        // M3: Source GET_PARAMETER → we reply with capabilities.
        let data = self
            .recv_message(&mut sock)
            .ok_or("No M3 received from source")?;
        log::info!("M3 received (capability query)");
        let m3_cseq = parse_cseq(&data);
        let body = self.build_capability_body(&data);
        let m3 = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {m3_cseq}\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(m3.as_bytes()).map_err(oserr)?;

        // M4: Source SET_PARAMETER (chosen params) → OK.
        let data = self
            .recv_message(&mut sock)
            .ok_or("No M4 received from source")?;
        log::info!("M4 received (parameters set)");
        let m4_cseq = parse_cseq(&data);
        self.parse_m4_params(&data);
        let m4 = format!("RTSP/1.0 200 OK\r\nCSeq: {m4_cseq}\r\n\r\n");
        sock.write_all(m4.as_bytes()).map_err(oserr)?;

        // M5: Source SET_PARAMETER (trigger SETUP) → OK.
        let data = self
            .recv_message(&mut sock)
            .ok_or("No M5 received from source")?;
        log::info!("M5 received (trigger SETUP)");
        let m5_cseq = parse_cseq(&data);
        let m5 = format!("RTSP/1.0 200 OK\r\nCSeq: {m5_cseq}\r\n\r\n");
        sock.write_all(m5.as_bytes()).map_err(oserr)?;

        // M6: We send SETUP.
        cseq += 1;
        let rtp_port = self.rtp_port.load(Ordering::SeqCst);
        let m6 = format!(
            "SETUP rtsp://{}/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: {cseq}\r\nTransport: RTP/AVP/UDP;unicast;client_port={rtp_port}\r\n\r\n",
            self.source_ip
        );
        sock.write_all(m6.as_bytes()).map_err(oserr)?;
        let data = self
            .recv_message(&mut sock)
            .ok_or("No M6 response from source")?;
        let session_id = parse_session_id(&data);
        let server_port = parse_server_port(&data);
        log::info!("Session: {session_id}, server_port: {server_port}");

        // Start the pipeline BEFORE sending PLAY.
        self.start_pipeline();

        // M7: We send PLAY.
        cseq += 1;
        let m7 = format!(
            "PLAY rtsp://{}/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {session_id}\r\n\r\n",
            self.source_ip
        );
        sock.write_all(m7.as_bytes()).map_err(oserr)?;
        self.recv_message(&mut sock)
            .ok_or("No M7 response from source")?;
        log::info!("M7 response received — streaming active!");

        let _ = self.events.send(Event::StreamStarted);

        // Streaming phase.
        self.streaming_loop(&mut sock, &session_id, cseq);
        Ok(())
    }

    /// Connect to the source's RTSP server with retries.
    fn connect_to_source(&self) -> Option<TcpStream> {
        let deadline = Instant::now() + RTSP_CONNECT_TIMEOUT;
        let addr = format!("{}:{}", self.source_ip, self.rtsp_port);
        let mut attempt = 0;
        while self.state.running.load(Ordering::SeqCst) && Instant::now() < deadline {
            attempt += 1;
            match addr
                .parse()
                .ok()
                .and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_secs(5)).ok())
            {
                Some(sock) => {
                    let _ = sock.set_nodelay(true);
                    let _ = sock.set_read_timeout(Some(RTSP_RECV_TIMEOUT));
                    log::info!("Connected to source RTSP server {addr} (attempt {attempt})");
                    return Some(sock);
                }
                None => {
                    log::debug!("RTSP connect attempt {attempt} failed — retrying in 1s");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
        if self.state.running.load(Ordering::SeqCst) {
            let msg =
                format!("Failed to connect to source RTSP at {addr} after {attempt} attempts");
            log::error!("{msg}");
            let _ = self.events.send(Event::StreamError(msg));
        }
        None
    }

    /// Streaming phase — keep-alive and teardown detection.
    fn streaming_loop(&self, sock: &mut TcpStream, _session_id: &str, _cseq: i64) {
        let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
        let mut buf = [0u8; RTSP_BUFFER_SIZE];

        while self.state.running.load(Ordering::SeqCst) {
            match sock.read(&mut buf) {
                Ok(0) => {
                    log::info!("Source closed RTSP connection");
                    self.stop_pipeline_and_emit();
                    return;
                }
                Ok(n) => {
                    let data = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let first_line = data.split("\r\n").next().unwrap_or("");
                    if data.contains("wfd_trigger_method: TEARDOWN")
                        || first_line.contains("TEARDOWN")
                    {
                        log::info!("Received TEARDOWN from source");
                        let cseq = parse_cseq(&data);
                        let resp = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n");
                        let _ = sock.write_all(resp.as_bytes());
                        self.stop_pipeline_and_emit();
                        return;
                    }
                    if data.contains("GET_PARAMETER") || data.contains("SET_PARAMETER") {
                        let cseq = parse_cseq(&data);
                        let resp = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n");
                        let _ = sock.write_all(resp.as_bytes());
                        if data.contains("wfd_video_formats") {
                            log::info!("Source sent updated video formats");
                        }
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    if self.state.running.load(Ordering::SeqCst) {
                        log::error!("Socket error in streaming loop: {e}");
                        self.stop_pipeline_and_emit();
                    }
                    return;
                }
            }
        }
    }

    /// Receive a complete RTSP message (honouring Content-Length).
    fn recv_message(&self, sock: &mut TcpStream) -> Option<String> {
        let mut buf = vec![0u8; RTSP_BUFFER_SIZE];
        let n = match sock.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => n,
            Err(_) => return None,
        };
        let mut data = buf[..n].to_vec();

        let text = String::from_utf8_lossy(&data);
        let mut content_length = 0usize;
        for line in text.split("\r\n") {
            if line.to_lowercase().starts_with("content-length:") {
                if let Some((_, v)) = line.split_once(':') {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                break;
            }
        }

        if content_length > 0 {
            if let Some(header_end) = find_subslice(&data, b"\r\n\r\n") {
                let body_start = header_end + 4;
                let mut body_received = data.len().saturating_sub(body_start);
                while body_received < content_length {
                    let want = (content_length - body_received).min(RTSP_BUFFER_SIZE);
                    let mut more = vec![0u8; want];
                    match sock.read(&mut more) {
                        Ok(0) | Err(_) => break,
                        Ok(m) => {
                            data.extend_from_slice(&more[..m]);
                            body_received += m;
                        }
                    }
                }
            }
        }
        Some(String::from_utf8_lossy(&data).into_owned())
    }

    /// Build the M3 capability response body (matches lazycast's working values).
    fn build_capability_body(&self, m3_data: &str) -> String {
        let rtp_port = self.rtp_port.load(Ordering::SeqCst);
        let mut msg =
            format!("wfd_client_rtp_ports: RTP/AVP/UDP;unicast {rtp_port} 0 mode=play\r\n");
        msg.push_str("wfd_audio_codecs: AAC 00000001 00\r\n");
        let (mw, mh) = *self.max_resolution.lock_safe();
        let vfmt = crate::rtsp::WfdVideoFormat::for_max_resolution(mw, mh).to_wfd_string();
        msg.push_str(&format!("wfd_video_formats: {vfmt}\r\n"));
        msg.push_str("wfd_3d_video_formats: none\r\n");
        msg.push_str("wfd_coupled_sink: none\r\n");
        msg.push_str("wfd_connector_type: 05\r\n");
        msg.push_str("wfd_uibc_capability: none\r\n");
        msg.push_str("wfd_standby_resume_capability: none\r\n");
        msg.push_str("wfd_content_protection: none\r\n");
        if m3_data.contains("wfd_idr_request_capability") {
            msg.push_str("wfd_idr_request_capability: 1\r\n");
        }
        msg
    }

    /// Parse M4 SET_PARAMETER for chosen stream parameters.
    fn parse_m4_params(&self, data: &str) {
        let body = match data.split_once("\r\n\r\n") {
            Some((_, b)) => b,
            None => return,
        };
        for line in body.split("\r\n") {
            if line.starts_with("wfd_video_formats:") {
                log::debug!("M4 video formats: {line}");
            } else if line.starts_with("wfd_audio_codecs:") {
                let codec = if line.contains("LPCM") { "LPCM" } else { "AAC" };
                *self.audio_codec.lock_safe() = codec.to_string();
                log::debug!("M4 audio codec: {codec}");
            } else if line.starts_with("wfd_client_rtp_ports:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    if let Ok(port) = parts[2].parse::<i64>() {
                        if (1024..=65535).contains(&port) {
                            self.rtp_port.store(port, Ordering::SeqCst);
                            log::info!("M4 set RTP port to {port}");
                        }
                    }
                }
            }
        }
    }

    /// Fast, up-front probe: can ANY audio sink actually open on this host/run
    /// context? pulsesink connects to PulseAudio at the READY transition, which
    /// fails ("Connection refused") when there is no user PA session (e.g. under
    /// sudo). We test that cheaply — set a standalone sink to READY with a short
    /// wait — so the real pipeline is built audio-on ONLY when audio can work,
    /// avoiding a failed PLAYING attempt that would delay the RTSP M7 response.
    pub(crate) fn audio_sink_available() -> bool {
        audio_sink_available()
    }

    /// Build and start the GStreamer pipeline, wiring bus + probe + stats.
    fn start_pipeline(&self) {
        let builder = PipelineBuilder::new(self.headless);
        let video_codec = self.video_codec.lock_safe().clone();
        let audio_codec = if self.audio_enabled {
            self.audio_codec.lock_safe().clone()
        } else {
            "AAC".to_string()
        };
        let use_hw = self.state.use_hw_decode.load(Ordering::SeqCst);
        let rtp_port = self.rtp_port.load(Ordering::SeqCst);

        // Build → wire bus → PLAYING. Two fallback axes, tried inline because a
        // failed PLAYING is a SYNCHRONOUS StateChangeError the bus-watch misses:
        //   • decode: HW (vaapi/nvh264) → SW (avdec_h264)
        //   • audio:  with the audio branch → video-only
        // Audio under sudo is unreliable (no PA session; pulsesink fails at
        // PLAYING with "Connection refused", and even autoaudiosink's backends
        // — PipeWire/ALSA/openal — can fail). The up-front probe is only a HINT
        // (it can pick a different sink than the pipeline), so we ALSO keep a
        // reactive video-only fallback. Attempts are ordered so video-only
        // always follows an audio attempt for the same decoder, and the whole
        // thing stays fast so the RTSP M7/PLAY response is not delayed.
        let want_audio = self.audio_enabled && Self::audio_sink_available();
        if self.audio_enabled && !want_audio {
            log::info!("Audio probe: no working audio sink — will stream video-only");
        }
        let mut attempts: Vec<(bool, bool)> = Vec::new();
        for &hw in if use_hw {
            &[true, false][..]
        } else {
            &[false][..]
        } {
            if want_audio {
                attempts.push((hw, true));
            }
            attempts.push((hw, false));
        }
        let mut pipeline: Option<gst::Pipeline> = None;
        let n_attempts = attempts.len();
        for (i, &(hw, audio)) in attempts.iter().enumerate() {
            let built =
                match builder.build_pipeline(rtp_port, &video_codec, &audio_codec, audio, hw) {
                    Ok(p) => p,
                    Err(e) => {
                        let msg = format!("Failed to start pipeline: {e}");
                        log::error!("{msg}");
                        self.state.errors.fetch_add(1, Ordering::SeqCst);
                        let _ = self.events.send(Event::StreamError(msg));
                        return;
                    }
                };

            // Wire the bus watch (async decode ERRORS, EOS, state) before PLAYING.
            let bus = built.bus().expect("pipeline has a bus");
            {
                let state = Arc::clone(&self.state);
                let events = self.events.clone();
                let pipeline_arc = Arc::clone(&self.pipeline);
                let epoch = self.epoch;
                let _ = bus.add_watch(move |_bus, msg| {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Error(err) => {
                            let emsg = format!("Pipeline error: {}", err.error());
                            log::error!("{emsg} (debug: {:?})", err.debug());
                            state.errors.fetch_add(1, Ordering::SeqCst);
                            if state.use_hw_decode.load(Ordering::SeqCst)
                                && err.error().to_string().to_lowercase().contains("decode")
                            {
                                log::warn!("Attempting software decode fallback");
                                state.use_hw_decode.store(false, Ordering::SeqCst);
                                if let Some(p) = pipeline_arc.lock_safe().take() {
                                    let _ = p.set_state(gst::State::Null);
                                }
                                let _ = epoch;
                            } else {
                                let _ = events.send(Event::StreamError(emsg));
                            }
                        }
                        MessageView::Eos(_) => {
                            log::info!("Pipeline received EOS");
                            if let Some(p) = pipeline_arc.lock_safe().take() {
                                let _ = p.set_state(gst::State::Null);
                            }
                            let _ = events.send(Event::StreamStopped(ReceiverStats::default()));
                            state.running.store(false, Ordering::SeqCst);
                        }
                        MessageView::StateChanged(_) => {}
                        _ => {}
                    }
                    gst::glib::ControlFlow::Continue
                });
            }

            // A live pipeline (udpsrc + tsdemux with dynamic pads) returns
            // StateChangeSuccess::Async immediately — the transition only
            // settles once data flows and the demuxer's pads are added/linked.
            // Do NOT treat the immediate return as final: set PLAYING, then wait
            // briefly on state() for the real outcome. Keep the wait SHORT (3s):
            // a live source returns NoPreroll almost immediately, and the RTSP
            // M7/PLAY response that follows must not be delayed past the
            // source's timeout.
            let start = built.set_state(gst::State::Playing);
            let reached = match start {
                Err(gst::StateChangeError) => false,
                Ok(_) => built.state(gst::ClockTime::from_seconds(3)).0.is_ok(),
            };
            if reached {
                log::info!(
                    "Pipeline reached PLAYING ({} decode{})",
                    if hw { "hardware" } else { "software" },
                    if audio { "" } else { ", video-only" }
                );
                if !audio && want_audio {
                    log::warn!(
                        "Audio disabled: the audio sink could not open (no PulseAudio \
                         session? — running under sudo drops the user audio session)"
                    );
                }
                pipeline = Some(built);
                break;
            }

            // Transition failed. Drain the bus for the real reason and decide
            // whether the AUDIO branch is the culprit.
            let mut audio_culprit = false;
            if let Some(bus) = built.bus() {
                while let Some(msg) = bus.timed_pop(gst::ClockTime::ZERO) {
                    if let gst::MessageView::Error(err) = msg.view() {
                        let dbg = format!("{:?}", err.debug());
                        if dbg.contains("pulsesink")
                            || dbg.contains("audiosink")
                            || dbg.contains("alsasink")
                        {
                            audio_culprit = true;
                        }
                        log::error!(
                            "PLAYING failure ({} decode, audio={}): {} (debug: {})",
                            if hw { "hardware" } else { "software" },
                            audio,
                            err.error(),
                            dbg
                        );
                    }
                }
            }
            let _ = built.set_state(gst::State::Null);

            // If the audio branch killed it and this attempt still had audio on,
            // jump straight to the matching audio-off attempt for this decoder.
            if audio && audio_culprit {
                log::warn!("Audio sink failed — retrying video-only");
            } else if hw && i + 1 < n_attempts {
                log::warn!(
                    "Hardware decoder failed to reach PLAYING — retrying with software decode"
                );
                self.state.use_hw_decode.store(false, Ordering::SeqCst);
            }
        }

        let pipeline = match pipeline {
            Some(p) => p,
            None => {
                let msg = "Pipeline failed to transition to PLAYING (hardware and software \
                           decode both failed)"
                    .to_string();
                log::error!("{msg}");
                self.state.errors.fetch_add(1, Ordering::SeqCst);
                let _ = self.events.send(Event::StreamError(msg));
                return;
            }
        };

        self.state
            .last_rtp_ns
            .store(self.epoch.elapsed().as_nanos() as u64, Ordering::SeqCst);
        log::info!("GStreamer pipeline started, listening on UDP port {rtp_port}");

        // Buffer pad-probe on udpsrc src to track RTP arrival for stream health.
        if let Some(udpsrc) = pipeline.by_name("udpsrc") {
            if let Some(src_pad) = udpsrc.static_pad("src") {
                let state = Arc::clone(&self.state);
                let epoch = self.epoch;
                src_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                    if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
                        state
                            .data_received
                            .fetch_add(buffer.size() as u64, Ordering::SeqCst);
                        state
                            .last_rtp_ns
                            .store(epoch.elapsed().as_nanos() as u64, Ordering::SeqCst);
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }

        // Caps probe on the decoder src pad: log the NEGOTIATED decoded
        // resolution (proves the source is sending HD, not upscaled 640x480),
        // populate state.resolution (previously always 0x0), and emit
        // ResolutionChanged for the UI.
        if let Some(dec) = pipeline.by_name("videodec") {
            if let Some(src_pad) = dec.static_pad("src") {
                let state = Arc::clone(&self.state);
                let events = self.events.clone();
                src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                    if let Some(gst::PadProbeData::Event(ref ev)) = info.data {
                        if let gst::EventView::Caps(c) = ev.view() {
                            let caps = c.caps();
                            if let Some(s) = caps.structure(0) {
                                if let (Ok(w), Ok(h)) =
                                    (s.get::<i32>("width"), s.get::<i32>("height"))
                                {
                                    let (w, h) = (w as u32, h as u32);
                                    let mut cur = state.resolution.lock_safe();
                                    if *cur != (w, h) {
                                        *cur = (w, h);
                                        drop(cur);
                                        log::info!("Negotiated video resolution: {w}x{h}");
                                        let _ = events.send(Event::ResolutionChanged {
                                            width: w,
                                            height: h,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }

        *self.pipeline.lock_safe() = Some(pipeline);

        // Stats monitor thread.
        let stats_ctx = StatsCtx {
            state: Arc::clone(&self.state),
            events: self.events.clone(),
            pipeline: Arc::clone(&self.pipeline),
            epoch: self.epoch,
            start_time: Arc::clone(&self.start_time),
        };
        std::thread::Builder::new()
            .name("stats-monitor".to_string())
            .spawn(move || stats_ctx.run())
            .expect("spawn stats-monitor");
    }

    fn stop_pipeline_and_emit(&self) {
        if let Some(p) = self.pipeline.lock_safe().take() {
            let _ = p.set_state(gst::State::Null);
        }
        let stats = build_stats(&self.state, &self.start_time, &self.video_codec);
        self.state.running.store(false, Ordering::SeqCst);
        let _ = self.events.send(Event::StreamStopped(stats));
    }
}

/// Stats collection thread running at 1-second intervals.
struct StatsCtx {
    state: Arc<RxState>,
    events: EventSender,
    pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    epoch: Instant,
    start_time: Arc<Mutex<Option<chrono::DateTime<Local>>>>,
}

impl StatsCtx {
    fn run(self) {
        let mut last_bytes: u64 = 0;
        let mut frame_history: Vec<(Instant, i64, i64)> = Vec::new();

        while self.state.running.load(Ordering::SeqCst) && self.pipeline.lock_safe().is_some() {
            std::thread::sleep(STATS_INTERVAL);
            if !self.state.running.load(Ordering::SeqCst) || self.pipeline.lock_safe().is_none() {
                break;
            }

            let now = Instant::now();
            let current_bytes = self.state.data_received.load(Ordering::SeqCst);
            let bytes_delta = current_bytes.saturating_sub(last_bytes);
            let bitrate = bytes_delta as f64 * 8.0;
            last_bytes = current_bytes;

            self.state
                .current_bitrate_milli
                .store((bitrate * 1000.0) as u64, Ordering::SeqCst);
            let peak = self.state.peak_bitrate_milli.load(Ordering::SeqCst);
            if (bitrate * 1000.0) as u64 > peak {
                self.state
                    .peak_bitrate_milli
                    .store((bitrate * 1000.0) as u64, Ordering::SeqCst);
            }

            let decoded = self.state.frames_decoded.load(Ordering::SeqCst);
            let dropped = self.state.frames_dropped.load(Ordering::SeqCst);
            frame_history.push((now, decoded, dropped));
            let cutoff = now - FRAME_DROP_WINDOW;
            frame_history.retain(|(t, _, _)| *t >= cutoff);
            if frame_history.len() >= 2 {
                let first = frame_history[0];
                let last = *frame_history.last().unwrap();
                let decoded_delta = last.1 - first.1;
                let dropped_delta = last.2 - first.2;
                if decoded_delta > 0 {
                    let drop_rate = dropped_delta as f64 / (decoded_delta + dropped_delta) as f64;
                    if drop_rate > FRAME_DROP_WARNING_THRESHOLD {
                        log::warn!(
                            "Frame drop rate {:.1}% exceeds threshold",
                            drop_rate * 100.0
                        );
                    }
                }
            }

            // Stream-loss check.
            let last_rtp_ns = self.state.last_rtp_ns.load(Ordering::SeqCst);
            if last_rtp_ns > 0 {
                let now_ns = self.epoch.elapsed().as_nanos() as u64;
                let silence = Duration::from_nanos(now_ns.saturating_sub(last_rtp_ns));
                if silence >= RTP_TIMEOUT {
                    let msg = format!("Stream lost: no RTP data for {:.1}s", silence.as_secs_f64());
                    log::error!("{msg}");
                    if let Some(p) = self.pipeline.lock_safe().take() {
                        let _ = p.set_state(gst::State::Null);
                    }
                    let _ = self.events.send(Event::StreamError(msg));
                    return;
                }
            }

            let duration = self
                .start_time
                .lock_safe()
                .map(|s| (Local::now() - s).num_seconds().max(0))
                .unwrap_or(0);
            let _ = self.events.send(Event::StatsUpdated(StreamStats {
                bitrate,
                peak_bitrate: self.state.peak_bitrate_milli.load(Ordering::SeqCst) as f64 / 1000.0,
                frames_decoded: decoded,
                frames_dropped: dropped,
                resolution: *self.state.resolution.lock_safe(),
                data_received: current_bytes as i64,
                duration,
            }));
        }
    }
}

fn oserr(e: std::io::Error) -> String {
    format!("RTSP session error: {e}")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_cseq(data: &str) -> i64 {
    for line in data.split("\r\n") {
        if line.to_lowercase().starts_with("cseq:") {
            if let Some((_, v)) = line.split_once(':') {
                if let Ok(n) = v.trim().parse() {
                    return n;
                }
            }
        }
    }
    0
}

fn parse_session_id(data: &str) -> String {
    for line in data.split("\r\n") {
        if line.to_lowercase().starts_with("session:") {
            if let Some((_, v)) = line.split_once(':') {
                return v.trim().split(';').next().unwrap_or("0").trim().to_string();
            }
        }
    }
    "0".to_string()
}

fn parse_server_port(data: &str) -> String {
    for line in data.split("\r\n") {
        if line.to_lowercase().starts_with("transport:") {
            for part in line.split(';') {
                if part.contains("server_port=") {
                    if let Some((_, v)) = part.split_once('=') {
                        return v.trim().to_string();
                    }
                }
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cseq_parsing() {
        assert_eq!(parse_cseq("OPTIONS * RTSP/1.0\r\nCSeq: 42\r\n\r\n"), 42);
        assert_eq!(parse_cseq("no cseq here"), 0);
    }

    #[test]
    fn session_id_parsing() {
        assert_eq!(
            parse_session_id("RTSP/1.0 200 OK\r\nSession: 1234ABCD;timeout=30\r\n\r\n"),
            "1234ABCD"
        );
        assert_eq!(
            parse_session_id("RTSP/1.0 200 OK\r\nSession: 55\r\n\r\n"),
            "55"
        );
        assert_eq!(parse_session_id("no session"), "0");
    }

    #[test]
    fn server_port_parsing() {
        let d = "RTSP/1.0 200 OK\r\nTransport: RTP/AVP/UDP;unicast;client_port=1028;server_port=5000\r\n\r\n";
        assert_eq!(parse_server_port(d), "5000");
    }

    #[test]
    fn capability_body_matches_python_lazycast_values() {
        gst_init();
        let (tx, _rx) = crate::events::channel();
        let ctx = SessionCtx {
            source_ip: "192.168.173.80".into(),
            rtsp_port: 7236,
            rtp_port: Arc::new(AtomicI64::new(1028)),
            headless: true,
            audio_enabled: true,
            events: tx,
            max_resolution: Arc::new(Mutex::new((1920, 1080))),
            state: Arc::new(RxState::new()),
            pipeline: Arc::new(Mutex::new(None)),
            epoch: Instant::now(),
            source_info: Arc::new(Mutex::new(None)),
            start_time: Arc::new(Mutex::new(None)),
            video_codec: Arc::new(Mutex::new("H264".into())),
            audio_codec: Arc::new(Mutex::new("AAC".into())),
        };
        let body = ctx.build_capability_body("");
        assert!(body.contains("wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play\r\n"));
        assert!(body.contains("wfd_audio_codecs: AAC 00000001 00\r\n"));
        // Now capability-driven: max_resolution (1920,1080) → 1080p native+bitmap.
        assert!(body.contains(
            "wfd_video_formats: 38 00 02 10 0001DFFF 00000000 00000000 00 0000 0000 00 none none\r\n"
        ));
        assert!(body.contains("wfd_connector_type: 05\r\n"));
        // IDR only appears when the source queried for it.
        assert!(!body.contains("wfd_idr_request_capability"));
        let body2 = ctx.build_capability_body("wfd_idr_request_capability");
        assert!(body2.contains("wfd_idr_request_capability: 1\r\n"));
    }
}
