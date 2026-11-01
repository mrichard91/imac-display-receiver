//! Receiver end-to-end over loopback (PLAN R4, R6): the built binary runs
//! with `fakesink` and the software decoder; each test drives it with a
//! minimal in-process sender speaking the protocol.

use std::io::{BufRead, BufReader};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use gst::prelude::*;
use imac_display_rx::net::{FramedReader, FramedWriter, Inbound};
use imac_display_rx::protocol::{
    CAPS_SPEED_TEST, CAPS_STATS, FFLAG_KEYFRAME, FrameHeader, Hello, Message, PingPong, Reason,
    SPEED_TEST_ABORTED, SPEED_TEST_OK, SpeedTestEnd, SpeedTestStart, Stats, StreamStart,
};

const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/imac5k-drm");

fn init_gst() {
    static INIT: Once = Once::new();
    INIT.call_once(|| gst::init().expect("gst init"));
}

/// The receiver binary on an ephemeral port, its stderr captured.
struct Rx {
    child: Child,
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Rx {
    fn spawn() -> Rx {
        Rx::spawn_with(&["--sink", "fakesink"])
    }

    /// `args` must include `--sink`.
    fn spawn_with(args: &[&str]) -> Rx {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_imac-display-rx"));
        cmd.args(["--port", "0", "--decoder", "avdec_h265"])
            .args(args)
            .args([
                "--log-level",
                "debug",
                "--name",
                "test-rx",
                "--drm-sysfs",
                SNAPSHOT,
            ])
            .stderr(Stdio::piped())
            .stdout(Stdio::null());
        for var in [
            "PANEL_WIDTH",
            "PANEL_HEIGHT",
            "PANEL_REFRESH_MHZ",
            "PANEL_MM",
        ] {
            cmd.env_remove(var);
        }
        let mut child = cmd.spawn().expect("spawn receiver");
        let stderr = child.stderr.take().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (port_tx, port_rx) = mpsc::channel();
        {
            let log = log.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("rx| {line}");
                    if let Some(rest) = line.split("listening on ").nth(1) {
                        let addr = rest.split_whitespace().next().unwrap_or_default();
                        let port = addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok());
                        let _ = port_tx.send(port);
                    }
                    log.lock().unwrap().push(line);
                }
            });
        }
        let port = port_rx
            .recv_timeout(Duration::from_secs(10))
            .ok()
            .flatten()
            .expect("receiver did not report its port");
        Rx { child, port, log }
    }

    fn count(&self, needle: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(needle))
            .count()
    }

    fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            if self.count(needle) > 0 {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn connect(&self) -> (Client, Message) {
        Client::connect(self.port)
    }
}

impl Drop for Rx {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum Event {
    Msg(Message, Instant),
    Closed(Instant),
}

/// A minimal sender: a reader thread turning messages into events, and a
/// heartbeat thread that can be paused to simulate a hung peer.
struct Client {
    writer: Arc<Mutex<FramedWriter<TcpStream>>>,
    events: Receiver<Event>,
    heartbeats: Arc<AtomicBool>,
    socket: TcpStream,
}

impl Client {
    /// Connects and returns the receiver's first message (HELLO or GOODBYE).
    fn connect(port: u16) -> (Client, Message) {
        init_gst();
        let socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        socket.set_nodelay(true).unwrap();
        let mut reader = FramedReader::new(socket.try_clone().unwrap());
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let first = match reader.read().expect("first message") {
            Inbound::Message(m) => m,
            other => panic!("unexpected {other:?}"),
        };
        socket.set_read_timeout(None).unwrap();

        let (tx, events) = mpsc::channel();
        thread::spawn(move || {
            loop {
                match reader.read() {
                    Ok(Inbound::Message(m)) => {
                        if tx.send(Event::Msg(m, Instant::now())).is_err() {
                            return;
                        }
                    }
                    Ok(other) => panic!("receiver sent {other:?}"),
                    Err(_) => {
                        let _ = tx.send(Event::Closed(Instant::now()));
                        return;
                    }
                }
            }
        });

        let writer = Arc::new(Mutex::new(FramedWriter::new(socket.try_clone().unwrap())));
        let heartbeats = Arc::new(AtomicBool::new(true));
        {
            let (writer, heartbeats) = (writer.clone(), heartbeats.clone());
            thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(200));
                    if heartbeats.load(Ordering::SeqCst) {
                        let msg = Message::Heartbeat { mono_us: 0 };
                        if writer.lock().unwrap().send(&msg).is_err() {
                            return;
                        }
                    }
                }
            });
        }
        let client = Client {
            writer,
            events,
            heartbeats,
            socket,
        };
        (client, first)
    }

    fn send(&self, msg: &Message) {
        self.writer.lock().unwrap().send(msg).expect("send");
    }

    fn send_frame(&self, pts_us: u64, keyframe: bool, au: &[u8]) {
        let header = FrameHeader {
            pts_us,
            fflags: if keyframe { FFLAG_KEYFRAME } else { 0 },
        };
        self.writer
            .lock()
            .unwrap()
            .send_frame(header, au)
            .expect("send frame");
    }

    /// Waits for the first message `f` accepts. Panics on close or timeout.
    fn expect<T>(
        &self,
        timeout: Duration,
        what: &str,
        mut f: impl FnMut(&Message) -> Option<T>,
    ) -> T {
        let until = Instant::now() + timeout;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(Event::Msg(m, _)) => {
                    if let Some(v) = f(&m) {
                        return v;
                    }
                }
                Ok(Event::Closed(_)) => panic!("connection closed while waiting for {what}"),
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }

    /// Collects messages until the receiver closes the connection.
    fn until_closed(&self, timeout: Duration) -> (Vec<(Message, Instant)>, Instant) {
        let until = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(Event::Msg(m, at)) => seen.push((m, at)),
                Ok(Event::Closed(at)) => return (seen, at),
                Err(_) => panic!("connection still open after {timeout:?}"),
            }
        }
    }

    fn stats_for(&self, stream_id: u32, timeout: Duration, done: impl Fn(&Stats) -> bool) -> Stats {
        self.expect(timeout, "STATS", |m| match m {
            Message::Stats(s) if s.stream_id == stream_id && done(s) => Some(*s),
            _ => None,
        })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

fn hello_of(msg: Message) -> Hello {
    match msg {
        Message::Hello(h) => h,
        other => panic!("expected HELLO, got {other:?}"),
    }
}

fn stream_start(stream_id: u32, width: u32, height: u32) -> Message {
    Message::StreamStart(StreamStart {
        protocol_version: 1,
        codec: 1,
        sflags: 0,
        width_px: width,
        height_px: height,
        fps_mhz: 60_000,
        stream_id,
    })
}

/// Encodes `frames` frames with x265 (Main, I420, no B-frames) into Annex-B
/// access units; only the first is a keyframe.
fn encode_clip(width: u32, height: u32, frames: u32) -> Vec<(Vec<u8>, bool)> {
    init_gst();
    assert!(
        gst::ElementFactory::find("x265enc").is_some(),
        "x265enc is required (gstreamer1.0-plugins-bad / gst-plugins-ugly)"
    );
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc num-buffers={frames} pattern=ball \
         ! video/x-raw,format=I420,width={width},height={height},framerate=60/1 \
         ! x265enc tune=zerolatency speed-preset=ultrafast bitrate=2000 key-int-max=100000 \
         ! video/x-h265,profile=main ! h265parse config-interval=-1 \
         ! video/x-h265,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let mut out = Vec::new();
    while let Ok(sample) = sink.pull_sample() {
        let buffer = sample.buffer().unwrap();
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        out.push((buffer.map_readable().unwrap().to_vec(), keyframe));
    }
    pipeline.set_state(gst::State::Null).unwrap();
    assert_eq!(out.len(), frames as usize);
    assert!(out[0].1, "first AU must be a keyframe");
    out
}

/// Sends a clip paced at 60 fps.
fn send_clip(client: &Client, clip: &[(Vec<u8>, bool)]) {
    let start = Instant::now();
    for (i, (au, key)) in clip.iter().enumerate() {
        let due = start + Duration::from_micros(16_667 * i as u64);
        thread::sleep(due.saturating_duration_since(Instant::now()));
        client.send_frame(1_000_000 + 16_667 * i as u64, *key, au);
    }
}

#[test]
fn hello_describes_the_snapshot_panel() {
    let rx = Rx::spawn();
    let (_client, first) = rx.connect();
    let hello = hello_of(first);
    assert_eq!(hello.protocol_version, 1);
    assert_eq!((hello.panel_width_px, hello.panel_height_px), (5120, 2880));
    assert_eq!(hello.refresh_mhz, 60_000);
    assert_eq!(
        (hello.physical_width_mm, hello.physical_height_mm),
        (597, 336)
    );
    assert_eq!(hello.codecs, 1);
    assert_eq!(hello.name, "test-rx");
    assert_eq!(
        hello.caps & (CAPS_STATS | CAPS_SPEED_TEST),
        CAPS_STATS | CAPS_SPEED_TEST
    );

    // display_id is stable across receiver restarts.
    drop(rx);
    let rx = Rx::spawn();
    let (_client, again) = rx.connect();
    assert_eq!(hello_of(again).display_id, hello.display_id);
}

#[test]
fn streams_frames_then_changes_resolution_and_reconnects() {
    let rx = Rx::spawn();
    let clip_a = encode_clip(320, 180, 180);
    let clip_b = encode_clip(640, 360, 60);
    let clip_c = encode_clip(480, 272, 30);

    let (client, _) = rx.connect();
    client.send(&stream_start(1, 320, 180));
    send_clip(&client, &clip_a);
    let s = client.stats_for(1, Duration::from_secs(5), |s| {
        s.frames_decoded - s.frames_dropped >= 150
    });
    assert!(s.frames_received >= 150, "{s:?}");
    assert!(rx.wait_for_log("first frame rendered (320x180)", Duration::from_secs(1)));

    // New geometry on the same connection: STREAM_STOP, STREAM_START, keyframe.
    client.send(&Message::StreamStop { stream_id: 1 });
    client.send(&stream_start(2, 640, 360));
    send_clip(&client, &clip_b);
    let s = client.stats_for(2, Duration::from_secs(5), |s| s.frames_decoded >= 50);
    assert!(s.frames_received <= 60, "counters reset per stream: {s:?}");
    assert!(rx.wait_for_log("first frame rendered (640x360)", Duration::from_secs(1)));

    client.send(&Message::goodbye(Reason::NORMAL, "switching"));
    client.until_closed(Duration::from_secs(2));
    drop(client);

    // A fresh connection at a third size, without restarting the receiver.
    let (client, first) = rx.connect();
    hello_of(first);
    client.send(&stream_start(1, 480, 272));
    send_clip(&client, &clip_c);
    assert!(rx.wait_for_log("first frame rendered (480x272)", Duration::from_secs(5)));
    assert_eq!(rx.count("first frame rendered"), 3);
}

#[test]
fn second_sender_gets_busy() {
    let rx = Rx::spawn();
    let (a, first) = rx.connect();
    hello_of(first);

    let (b, first) = rx.connect();
    match first {
        Message::Goodbye { reason, .. } => assert_eq!(reason, Reason::BUSY),
        other => panic!("expected GOODBYE BUSY, got {other:?}"),
    }
    b.until_closed(Duration::from_secs(2));

    // The first sender is unaffected.
    let ping = PingPong { seq: 1, t_us: 1 };
    a.send(&Message::Ping(ping));
    a.expect(Duration::from_secs(1), "PONG", |m| {
        (*m == Message::Pong(ping)).then_some(())
    });
}

#[test]
fn silent_sender_is_dropped_within_two_seconds() {
    let rx = Rx::spawn();
    let (client, _) = rx.connect();
    client.send(&stream_start(1, 320, 180));
    thread::sleep(Duration::from_millis(300));

    client.heartbeats.store(false, Ordering::SeqCst);
    let paused_at = Instant::now();
    let (seen, closed_at) = client.until_closed(Duration::from_secs(3));
    let after = closed_at - paused_at;
    assert!(
        after >= Duration::from_millis(1700) && after <= Duration::from_millis(2500),
        "closed {after:?} after the sender went silent"
    );
    assert!(
        !seen
            .iter()
            .any(|(m, _)| matches!(m, Message::Goodbye { .. })),
        "no GOODBYE to a dead peer"
    );

    // Back to LISTENING within 1 s.
    thread::sleep(Duration::from_millis(1000));
    let (_client, first) = rx.connect();
    hello_of(first);
}

#[test]
fn ping_gets_pong_and_heartbeats_flow() {
    let rx = Rx::spawn();
    let (client, _) = rx.connect();
    let sent_at = Instant::now();
    let ping = PingPong {
        seq: 42,
        t_us: 123_456,
    };
    client.send(&Message::Ping(ping));
    client.expect(Duration::from_millis(500), "PONG", |m| {
        (*m == Message::Pong(ping)).then_some(())
    });
    eprintln!("PING round trip {:?}", sent_at.elapsed());

    // Heartbeats at least every 500 ms (with scheduling slack).
    let mut beats = Vec::new();
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        if let Ok(Event::Msg(Message::Heartbeat { .. }, at)) =
            client.events.recv_timeout(Duration::from_millis(100))
        {
            beats.push(at);
        }
    }
    assert!(beats.len() >= 4, "only {} heartbeats in 2 s", beats.len());
    let max_gap = beats.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(
        max_gap < Duration::from_millis(600),
        "heartbeat gap {max_gap:?}"
    );
}

#[test]
fn unknown_message_types_are_skipped() {
    let rx = Rx::spawn();
    let (client, _) = rx.connect();
    {
        use std::io::Write;
        let mut raw = imac_display_rx::protocol::Header::new(0x30, 3)
            .encode()
            .to_vec();
        raw.extend_from_slice(b"new");
        (&client.socket).write_all(&raw).unwrap();
    }
    let ping = PingPong { seq: 9, t_us: 9 };
    client.send(&Message::Ping(ping));
    client.expect(Duration::from_secs(1), "PONG", |m| {
        (*m == Message::Pong(ping)).then_some(())
    });
}

#[test]
fn frame_before_stream_start_is_a_protocol_error() {
    let rx = Rx::spawn();
    let (client, _) = rx.connect();
    client.send_frame(1, true, &[0, 0, 0, 1]);
    let reason = client.expect(Duration::from_secs(1), "GOODBYE", |m| match m {
        Message::Goodbye { reason, .. } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, Reason::PROTOCOL_ERROR);
    client.until_closed(Duration::from_secs(1));
}

#[test]
fn keyframe_requested_after_500ms_and_at_most_once_per_second() {
    let rx = Rx::spawn();
    let (client, _) = rx.connect();
    client.send(&stream_start(1, 320, 180));
    let started = Instant::now();
    // Delta frames only: all discarded while waiting for a keyframe.
    for i in 0..10 {
        client.send_frame(i * 16_667, false, &[0, 0, 0, 1, 0x02, 0x01]);
    }
    let mut requests = Vec::new();
    while started.elapsed() < Duration::from_millis(2700) {
        if let Ok(Event::Msg(Message::KeyframeRequest, at)) =
            client.events.recv_timeout(Duration::from_millis(50))
        {
            requests.push(at - started);
        }
    }
    assert!(
        (2..=3).contains(&requests.len()),
        "requests at {requests:?}"
    );
    assert!(requests[0] >= Duration::from_millis(450), "{requests:?}");
    for w in requests.windows(2) {
        assert!(w[1] - w[0] >= Duration::from_millis(950), "{requests:?}");
    }
}

#[test]
fn speed_test_over_loopback() {
    let rx = Rx::spawn();
    let (client, _) = rx.connect();

    let start = SpeedTestStart {
        test_id: 1,
        duration_ms: 500,
    };
    client.send(&Message::SpeedTestStart(start));
    let chunk = vec![0x5Au8; 256 * 1024];
    let mut sent = 0u64;
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        client
            .writer
            .lock()
            .unwrap()
            .send_speed_test_data(1, &chunk)
            .unwrap();
        sent += chunk.len() as u64;
    }
    // Odd sizes, an empty one, and one for another test that must not count.
    for len in [0usize, 1, 12_345] {
        client
            .writer
            .lock()
            .unwrap()
            .send_speed_test_data(1, &chunk[..len])
            .unwrap();
        sent += len as u64;
    }
    client
        .writer
        .lock()
        .unwrap()
        .send_speed_test_data(99, &chunk)
        .unwrap();
    client.send(&Message::SpeedTestEnd(SpeedTestEnd {
        test_id: 1,
        bytes_sent: sent,
    }));

    let result = client.expect(Duration::from_secs(6), "SPEED_TEST_RESULT", |m| match m {
        Message::SpeedTestResult(r) => Some(*r),
        _ => None,
    });
    assert_eq!(result.test_id, 1);
    assert_eq!(result.status, SPEED_TEST_OK);
    assert_eq!(result.bytes_received, sent);
    assert!(result.elapsed_us >= 400_000, "{result:?}");
    eprintln!(
        "loopback: {} bytes in {} us = {} Mbit/s",
        result.bytes_received,
        result.elapsed_us,
        result.kbit_per_s().unwrap() / 1000
    );

    // A new START aborts the running test and reports it.
    client.send(&Message::SpeedTestStart(SpeedTestStart {
        test_id: 2,
        duration_ms: 1000,
    }));
    client
        .writer
        .lock()
        .unwrap()
        .send_speed_test_data(2, &chunk)
        .unwrap();
    client.send(&Message::SpeedTestStart(SpeedTestStart {
        test_id: 3,
        duration_ms: 1000,
    }));
    let aborted = client.expect(Duration::from_secs(2), "aborted result", |m| match m {
        Message::SpeedTestResult(r) => Some(*r),
        _ => None,
    });
    assert_eq!(
        (aborted.test_id, aborted.status, aborted.bytes_received),
        (2, SPEED_TEST_ABORTED, chunk.len() as u64)
    );
    client.send(&Message::SpeedTestEnd(SpeedTestEnd {
        test_id: 3,
        bytes_sent: 0,
    }));
    let done = client.expect(Duration::from_secs(2), "result 3", |m| match m {
        Message::SpeedTestResult(r) => Some(*r),
        _ => None,
    });
    assert_eq!(
        (done.test_id, done.status, done.bytes_received),
        (3, SPEED_TEST_OK, 0)
    );
}

#[test]
fn stream_start_timeout_is_restarted_by_speed_test() {
    let rx = Rx::spawn();
    let (client, _) = rx.connect();
    let connected = Instant::now();

    thread::sleep(Duration::from_secs(3));
    client.send(&Message::SpeedTestStart(SpeedTestStart {
        test_id: 1,
        duration_ms: 100,
    }));
    client.send(&Message::SpeedTestEnd(SpeedTestEnd {
        test_id: 1,
        bytes_sent: 0,
    }));
    let restarted = Instant::now();

    let (reason, at) = client.expect(Duration::from_secs(8), "GOODBYE", |m| match m {
        Message::Goodbye { reason, .. } => Some((*reason, Instant::now())),
        _ => None,
    });
    assert_eq!(reason, Reason::TIMEOUT);
    let since_restart = at - restarted;
    assert!(
        since_restart >= Duration::from_millis(4800)
            && since_restart <= Duration::from_millis(5600),
        "GOODBYE {since_restart:?} after SPEED_TEST_END ({:?} after connect)",
        at - connected
    );
    client.until_closed(Duration::from_secs(1));
}

#[test]
fn idle_screen_hides_while_a_sender_is_connected() {
    let rx = Rx::spawn();
    assert!(rx.wait_for_log("idle screen up", Duration::from_secs(2)));
    let (client, _) = rx.connect();
    assert!(rx.wait_for_log("idle screen down", Duration::from_secs(1)));
    client.send(&Message::goodbye(Reason::NORMAL, "bye"));
    client.until_closed(Duration::from_secs(2));
    let until = Instant::now() + Duration::from_secs(2);
    while rx.count("idle screen up") < 2 && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(rx.count("idle screen up"), 2);
}

#[test]
fn sink_failure_while_appsrc_is_full_frees_the_receiver() {
    // `identity` as the sink has an unlinked src pad: the first decoded frame
    // fails with not-linked, appsrc's streaming task stops, and frames that
    // keep arriving fill it until the reader blocks in push.
    let rx = Rx::spawn_with(&["--sink", "identity", "--idle-screen", "false"]);
    let clip = encode_clip(320, 180, 120);
    let (client, first) = rx.connect();
    hello_of(first);
    client.send(&stream_start(1, 320, 180));
    // Back to back, as a sender does while TCP has room. Writes may fail
    // once the receiver closes; that is expected.
    for (i, (au, key)) in clip.iter().enumerate() {
        let header = FrameHeader {
            pts_us: 1_000_000 + 16_667 * i as u64,
            fflags: if *key { FFLAG_KEYFRAME } else { 0 },
        };
        if client
            .writer
            .lock()
            .unwrap()
            .send_frame(header, au)
            .is_err()
        {
            break;
        }
    }
    let (seen, closed_at) = client.until_closed(Duration::from_secs(5));
    assert!(
        seen.iter().any(|(m, _)| matches!(
            m,
            Message::Goodbye {
                reason: Reason::INTERNAL_ERROR,
                ..
            }
        )),
        "expected GOODBYE INTERNAL_ERROR, got {seen:?}"
    );
    drop(client);

    // §7.1: ready to accept again within 1 s.
    assert!(rx.wait_for_log("session ended", Duration::from_secs(1)));
    let (_client, first) = rx.connect();
    hello_of(first);
    assert!(closed_at.elapsed() < Duration::from_secs(2));
}
