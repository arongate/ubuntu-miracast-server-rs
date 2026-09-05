//! Integration test: drive the real `MiracastReceiver` RTSP client through the
//! full WFD M1–M7 sink handshake against a mock source on loopback.
//!
//! This exercises the actual socket I/O and RTSP message construction of
//! `receiver.rs` — the protocol layer that cannot be reached by unit tests —
//! without any Wi-Fi/P2P hardware. The mock plays the SOURCE side (the phone):
//! it accepts the sink's TCP connection on an ephemeral port, sends M1/M3/M4/M5,
//! and answers the sink's M2/M6/M7, following the sequence documented in
//! `receiver.rs`.
//!
//! Runs only on the headless build (`--no-default-features`) — it uses the
//! `fakesink` video path, so no display/GTK is required. GStreamer plugins must
//! be present on the host (they are in CI via the -dev packages).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use chrono::Local;
use ubuntu_miracast_server::events::{channel, Event};
use ubuntu_miracast_server::models::IncomingConnection;
use ubuntu_miracast_server::receiver::MiracastReceiver;

/// Read one complete RTSP message: loop until the header terminator is seen,
/// then read any declared Content-Length body. Retries on WouldBlock/TimedOut
/// within an overall deadline so it is robust against the timing differences
/// between the headless and GTK-linked builds (the sink sends each message when
/// its own state machine advances, not on a fixed cadence).
fn recv_message(sock: &mut TcpStream) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];

    let read_some = |sock: &mut TcpStream, buf: &mut [u8]| -> Option<usize> {
        loop {
            match sock.read(buf) {
                Ok(n) => return Some(n),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("mock source: read error: {e}"),
            }
        }
    };

    // Read until we have the header terminator.
    loop {
        match read_some(sock, &mut buf) {
            Some(0) | None => break,
            Some(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&data).into_owned();

    // If a body is declared, read the remainder.
    let content_length = text
        .split("\r\n")
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    if content_length > 0 {
        if let Some(idx) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let body_start = idx + 4;
            while data.len() - body_start < content_length {
                match read_some(sock, &mut buf) {
                    Some(0) | None => break,
                    Some(n) => data.extend_from_slice(&buf[..n]),
                }
            }
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

fn parse_cseq(msg: &str) -> i64 {
    for line in msg.split("\r\n") {
        if let Some(rest) = line.to_lowercase().strip_prefix("cseq:") {
            if let Ok(n) = rest.trim().parse() {
                return n;
            }
        }
    }
    0
}

/// Play the SOURCE side of the WFD handshake on an accepted connection.
fn run_mock_source(mut sock: TcpStream) {
    sock.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    // M1: source → OPTIONS ; sink replies 200 OK.
    sock.write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nRequire: org.wfa.wfd1.0\r\n\r\n")
        .unwrap();
    let _m1_reply = recv_message(&mut sock);

    // M2: sink → OPTIONS ; source replies 200 OK.
    let m2 = recv_message(&mut sock);
    let m2_cseq = parse_cseq(&m2);
    sock.write_all(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: {m2_cseq}\r\nPublic: org.wfa.wfd1.0, SET_PARAMETER, GET_PARAMETER, SETUP, PLAY, TEARDOWN\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();

    // M3: source → GET_PARAMETER (capability query) ; sink replies with WFD params.
    let body = "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n";
    sock.write_all(
        format!(
            "GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 2\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let m3_reply = recv_message(&mut sock);
    assert!(
        m3_reply.contains("wfd_client_rtp_ports"),
        "M3 reply must carry the sink capabilities, got: {m3_reply}"
    );

    // M4: source → SET_PARAMETER (chosen params) ; sink replies 200 OK.
    let m4_body = "wfd_video_formats: 00 00 02 10 0001FEFF 00000000 00000000 00 0000 0000 00 none none\r\nwfd_audio_codecs: AAC 00000001 00\r\nwfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play\r\n";
    sock.write_all(
        format!(
            "SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 3\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{m4_body}",
            m4_body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let _m4_reply = recv_message(&mut sock);

    // M5: source → SET_PARAMETER (trigger SETUP) ; sink replies 200 OK.
    let m5_body = "wfd_trigger_method: SETUP\r\n";
    sock.write_all(
        format!(
            "SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 4\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{m5_body}",
            m5_body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let _m5_reply = recv_message(&mut sock);

    // M6: sink → SETUP ; source replies with a Session id + server_port.
    let m6 = recv_message(&mut sock);
    assert!(m6.starts_with("SETUP "), "expected SETUP, got: {m6}");
    let m6_cseq = parse_cseq(&m6);
    sock.write_all(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: {m6_cseq}\r\nSession: 1234ABCD;timeout=30\r\nTransport: RTP/AVP/UDP;unicast;client_port=1028;server_port=5000\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();

    // M7: sink → PLAY ; source replies 200 OK. Streaming is now active.
    let m7 = recv_message(&mut sock);
    assert!(m7.starts_with("PLAY "), "expected PLAY, got: {m7}");
    assert!(
        m7.contains("Session: 1234ABCD"),
        "PLAY must echo the session id"
    );
    let m7_cseq = parse_cseq(&m7);
    sock.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {m7_cseq}\r\n\r\n").as_bytes())
        .unwrap();

    // Hold the connection briefly so the sink stays in the streaming loop.
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn rtsp_m1_to_m7_handshake_reaches_stream_started() {
    // Ephemeral loopback port standing in for the source's RTSP server (7236).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock source");
    let port = listener.local_addr().unwrap().port();

    let source = std::thread::spawn(move || {
        let (sock, _) = listener.accept().expect("accept sink connection");
        run_mock_source(sock);
    });

    let (tx, rx) = channel();
    // rtsp_port = mock port; headless=true → fakesink (no display needed).
    let mut receiver = MiracastReceiver::new(port, 1028, true, false, tx);

    let conn = IncomingConnection::try_new(
        "00:11:22:33:44:55",
        "127.0.0.1",
        "Mock Source",
        "p2p-test-0",
        "192.168.173.1",
        Local::now(),
        true,
    )
    .expect("valid connection");

    receiver.start_receiving(conn);

    // The receiver must emit StreamStarted after completing M1–M7.
    let mut saw_started = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::StreamStarted) => {
                saw_started = true;
                break;
            }
            Ok(Event::StreamError(e)) => panic!("unexpected stream error during handshake: {e}"),
            Ok(_) => {} // stats/other events are fine
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = receiver.stop_receiving();
    let _ = source.join();

    assert!(
        saw_started,
        "receiver should reach StreamStarted after the full M1–M7 handshake"
    );
}
