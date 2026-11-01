//! Stand-in for the Mac sender (PLAN R4), for exercising a receiver without a
//! Mac: streams a software-encoded (x265) test pattern, or runs the link
//! speed test of PROTOCOL.md §6.9.
//!
//!     cargo run --release --example test_sender -- --host 10.99.0.2
//!     cargo run --release --example test_sender -- --host 10.99.0.2 --speed-test 1000

use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use gst::prelude::*;

use imac_display_rx::net::{self, FramedReader, FramedWriter, Inbound};
use imac_display_rx::protocol::{
    CAPS_SPEED_TEST, CODEC_HEVC_MAIN, FFLAG_KEYFRAME, FrameHeader, Hello, Message,
    PROTOCOL_VERSION, Reason, SpeedTestEnd, SpeedTestResult, SpeedTestStart, StreamStart,
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(2);
const HEARTBEAT_IDLE: Duration = Duration::from_millis(400);
const SPEED_TEST_CHUNK: usize = 1024 * 1024;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 7878)]
    port: u16,
    /// Coded width; defaults to the receiver's panel width.
    #[arg(long)]
    width: Option<u32>,
    /// Coded height; defaults to the receiver's panel height.
    #[arg(long)]
    height: Option<u32>,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    #[arg(long, default_value_t = 8000)]
    bitrate_kbps: u32,
    /// Stop after this many seconds (default: run until the receiver leaves).
    #[arg(long)]
    seconds: Option<u64>,
    /// Run a speed test of this many milliseconds instead of streaming.
    #[arg(long, value_name = "MS")]
    speed_test: Option<u32>,
}

/// State shared by the main, reader and heartbeat threads.
struct Link {
    writer: Mutex<FramedWriter<TcpStream>>,
    socket: TcpStream,
    keyframe_requested: AtomicBool,
    done: AtomicBool,
    last_rx: Mutex<Instant>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Link {
    fn send(&self, msg: &Message) -> Result<()> {
        lock(&self.writer).send(msg)?;
        Ok(())
    }

    fn finish(&self) {
        if !self.done.swap(true, Ordering::SeqCst) {
            let _ = self.socket.shutdown(Shutdown::Both);
        }
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    env_logger::Builder::new().parse_filters("info").init();
    gst::init()?;

    let socket = TcpStream::connect((args.host.as_str(), args.port))
        .with_context(|| format!("connect {}:{}", args.host, args.port))?;
    net::configure(&socket)?;
    let mut reader = FramedReader::new(socket.try_clone()?);
    socket.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let hello = match reader.read()? {
        Inbound::Message(Message::Hello(h)) => h,
        Inbound::Message(Message::Goodbye { reason, message }) => {
            bail!("receiver said GOODBYE {reason}: {message:?}")
        }
        other => bail!("expected HELLO, got {other:?}"),
    };
    socket.set_read_timeout(None)?;
    print_hello(&hello);
    if hello.protocol_version != PROTOCOL_VERSION {
        bail!("receiver speaks protocol v{}", hello.protocol_version);
    }

    let link = Arc::new(Link {
        writer: Mutex::new(FramedWriter::new(socket.try_clone()?)),
        socket,
        keyframe_requested: AtomicBool::new(false),
        done: AtomicBool::new(false),
        last_rx: Mutex::new(Instant::now()),
    });
    let (results_tx, results_rx) = mpsc::channel();
    {
        let link = link.clone();
        thread::spawn(move || read_loop(&link, reader, results_tx));
    }
    {
        let link = link.clone();
        thread::spawn(move || heartbeat_loop(&link));
    }

    let outcome = match args.speed_test {
        Some(ms) => {
            if hello.caps & CAPS_SPEED_TEST == 0 {
                bail!("receiver does not advertise the speed test (caps bit 2)");
            }
            speed_test(&link, &results_rx, ms).map(|r| print_speed(&r))
        }
        None => stream(&link, &hello, &args),
    };
    let _ = link.send(&Message::goodbye(Reason::NORMAL, "test_sender done"));
    link.finish();
    outcome
}

fn print_hello(h: &Hello) {
    println!(
        "HELLO from {:?}: {}x{} @ {}.{:03} Hz, {}x{} mm, caps {:#06b}, id {:016x}",
        h.name,
        h.panel_width_px,
        h.panel_height_px,
        h.refresh_mhz / 1000,
        h.refresh_mhz % 1000,
        h.physical_width_mm,
        h.physical_height_mm,
        h.caps,
        h.display_id
    );
}

fn read_loop(link: &Link, mut reader: FramedReader<TcpStream>, results: Sender<SpeedTestResult>) {
    loop {
        let msg = match reader.read() {
            Ok(Inbound::Message(m)) => m,
            Ok(other) => {
                log::warn!("[net] unexpected {other:?} from receiver");
                continue;
            }
            Err(e) => {
                if !link.is_done() {
                    log::info!("[net] connection ended: {e}");
                }
                link.finish();
                return;
            }
        };
        *lock(&link.last_rx) = Instant::now();
        match msg {
            Message::KeyframeRequest => {
                log::info!("[net] KEYFRAME_REQUEST");
                link.keyframe_requested.store(true, Ordering::SeqCst);
            }
            Message::Ping(p) => {
                let _ = link.send(&Message::Pong(p));
            }
            Message::SpeedTestResult(r) => {
                let _ = results.send(r);
            }
            Message::Stats(s) => log::debug!("[net] {s:?}"),
            Message::Goodbye { reason, message } => {
                log::info!("[net] receiver said GOODBYE {reason}: {message:?}");
                link.finish();
                return;
            }
            _ => {}
        }
    }
}

fn heartbeat_loop(link: &Link) {
    while !link.is_done() {
        thread::sleep(Duration::from_millis(50));
        if lock(&link.last_rx).elapsed() >= LIVENESS_TIMEOUT {
            log::warn!("[net] receiver silent for {LIVENESS_TIMEOUT:?}; giving up");
            link.finish();
            return;
        }
        let idle = lock(&link.writer).idle_for(Instant::now()) >= HEARTBEAT_IDLE;
        if idle {
            let _ = link.send(&Message::Heartbeat {
                mono_us: net::mono_us(),
            });
        }
    }
}

/// PROTOCOL.md §6.9 sender procedure.
fn speed_test(
    link: &Link,
    results: &Receiver<SpeedTestResult>,
    duration_ms: u32,
) -> Result<SpeedTestResult> {
    let start = SpeedTestStart {
        test_id: 1,
        duration_ms,
    };
    start.validate()?;
    link.send(&Message::SpeedTestStart(start))?;
    let filler = vec![0u8; SPEED_TEST_CHUNK];
    let deadline = Instant::now() + Duration::from_millis(duration_ms.into());
    let mut bytes_sent = 0u64;
    // Blocking writes keep the backlog at the socket buffer size, so END is
    // not stuck behind a large queue.
    while Instant::now() < deadline && !link.is_done() {
        lock(&link.writer).send_speed_test_data(start.test_id, &filler)?;
        bytes_sent += filler.len() as u64;
    }
    link.send(&Message::SpeedTestEnd(SpeedTestEnd {
        test_id: start.test_id,
        bytes_sent,
    }))?;
    let wait = Duration::from_millis(duration_ms.into()) + Duration::from_secs(5);
    let until = Instant::now() + wait;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        let r = results
            .recv_timeout(left)
            .map_err(|_| anyhow!("no SPEED_TEST_RESULT within {wait:?}"))?;
        if r.test_id == start.test_id {
            println!("sent {bytes_sent} filler bytes");
            return Ok(r);
        }
    }
}

fn print_speed(r: &SpeedTestResult) {
    let status = if r.status == 0 { "ok" } else { "ABORTED" };
    let kbps = r.kbit_per_s().unwrap_or(0);
    println!(
        "speed test {} {status}: {} bytes in {:.1} ms = {}.{:03} Mbit/s",
        r.test_id,
        r.bytes_received,
        r.elapsed_us as f64 / 1000.0,
        kbps / 1000,
        kbps % 1000
    );
}

fn stream(link: &Link, hello: &Hello, args: &Args) -> Result<()> {
    let width = args.width.unwrap_or(hello.panel_width_px);
    let height = args.height.unwrap_or(hello.panel_height_px);
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc is-live=true pattern=ball \
         ! video/x-raw,format=I420,width={width},height={height},framerate={fps}/1 \
         ! x265enc tune=zerolatency speed-preset=ultrafast bitrate={kbps} key-int-max={kint} \
         ! video/x-h265,profile=main \
         ! h265parse config-interval=-1 \
         ! video/x-h265,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=4",
        fps = args.fps,
        kbps = args.bitrate_kbps,
        // TCP never loses data; keyframes only on demand (§6.4).
        kint = args.fps * 3600,
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    let appsink = pipeline
        .by_name("sink")
        .context("no appsink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("sink is not an appsink"))?;

    link.send(&Message::StreamStart(StreamStart {
        protocol_version: PROTOCOL_VERSION,
        codec: CODEC_HEVC_MAIN,
        sflags: 0,
        width_px: width,
        height_px: height,
        fps_mhz: args.fps * 1000,
        stream_id: 1,
    }))?;
    pipeline.set_state(gst::State::Playing)?;
    println!("streaming {width}x{height} @ {} fps", args.fps);

    let started = Instant::now();
    let mut last_pts_us = 0u64;
    let (mut frames, mut bytes, mut window) = (0u64, 0u64, Instant::now());
    let result = loop {
        if link.is_done()
            || args
                .seconds
                .is_some_and(|s| started.elapsed().as_secs() >= s)
        {
            break Ok(());
        }
        if link.keyframe_requested.swap(false, Ordering::SeqCst) {
            appsink.send_event(force_key_unit());
        }
        let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
            if appsink.is_eos() {
                break Err(anyhow!("encoder reached EOS"));
            }
            continue;
        };
        let buffer = sample.buffer().context("sample without buffer")?;
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        let pts_us = buffer
            .pts()
            .map_or(last_pts_us + 1, |p| p.useconds())
            .max(last_pts_us + 1);
        last_pts_us = pts_us;
        let map = buffer.map_readable()?;
        let header = FrameHeader {
            pts_us,
            fflags: if keyframe { FFLAG_KEYFRAME } else { 0 },
        };
        if let Err(e) = lock(&link.writer).send_frame(header, &map) {
            break Err(e.into());
        }
        frames += 1;
        bytes += map.len() as u64;
        if window.elapsed() >= Duration::from_secs(1) {
            let secs = window.elapsed().as_secs_f64();
            println!(
                "{:.1} fps, {:.2} Mbit/s",
                frames as f64 / secs,
                bytes as f64 * 8.0 / secs / 1e6
            );
            (frames, bytes, window) = (0, 0, Instant::now());
        }
    };
    let _ = link.send(&Message::StreamStop { stream_id: 1 });
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// GstVideoEncoder's upstream force-key-unit event, built by hand to avoid a
/// gstreamer-video dependency for this one structure.
fn force_key_unit() -> gst::Event {
    gst::event::CustomUpstream::new(
        gst::Structure::builder("GstForceKeyUnit")
            .field("all-headers", true)
            .field("count", 0u32)
            .build(),
    )
}
