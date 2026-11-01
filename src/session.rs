//! One sender connection: the receiver state machine (PROTOCOL.md §7.1).
//!
//! `Core` is the pure state machine: it takes inbound messages, pipeline
//! events and clock ticks and returns `Command`s. `run` is the I/O shell: a
//! reader thread (this one) feeds the socket into `Core` and pushes access
//! units into the pipeline; a ticker thread drives timers and heartbeats.
//! The two share `Live` behind a mutex that is never held across a blocking
//! socket read or an `appsrc` push.

use std::collections::VecDeque;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::net::{self, FramedReader, FramedWriter, Inbound, NetError};
use crate::pipeline::{CounterSnapshot, Counters, Pipeline, PipelineConfig, PipelineEvent};
use crate::protocol::{
    CODEC_HEVC_MAIN, FrameHeader, Hello, Message, PROTOCOL_VERSION, Reason, Stats, StreamStart,
};
use crate::slot::Slot;
use crate::speedtest::SpeedTestReceiver;

pub const LIVENESS_TIMEOUT: Duration = Duration::from_millis(2000);
/// Send a HEARTBEAT when nothing was sent for this long; with `TICK` this
/// keeps every gap under the 500 ms the protocol requires (§6.5).
pub const HEARTBEAT_IDLE: Duration = Duration::from_millis(400);
pub const STREAM_START_TIMEOUT: Duration = Duration::from_secs(5);
pub const KEYFRAME_WAIT: Duration = Duration::from_millis(500);
pub const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const STATS_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_DECODE_FAILURES: u32 = 3;
const TICK: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    HelloSent,
    WaitKeyframe,
    Streaming,
    Paused,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Send(Message),
    /// (Re)build the decode pipeline. `new_stream` resets the STATS counters.
    StartPipeline {
        width: u32,
        height: u32,
        new_stream: bool,
    },
    StopPipeline,
    /// Close the socket (after sending anything queued before it).
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVerdict {
    Push { pts_ns: u64 },
    Discard,
}

#[derive(Debug)]
struct ActiveStream {
    stream_id: u32,
    width: u32,
    height: u32,
    /// When the current wait for a keyframe began (STREAM_START or restart).
    wait_since: Instant,
    base_pts_us: Option<u64>,
    frames_received: u64,
}

#[derive(Debug)]
pub struct Core {
    state: State,
    panel: (u32, u32),
    stream: Option<ActiveStream>,
    last_stream_id: Option<u32>,
    stream_start_deadline: Instant,
    last_rx: Instant,
    last_keyframe_request: Option<Instant>,
    last_stats: Instant,
    /// Decode time totals at the previous STATS, for per-interval averages.
    stats_prev: (u64, u64),
    decode_failures: u32,
    decoded_at_last_failure: u64,
    speed: SpeedTestReceiver,
}

impl Core {
    /// State right after HELLO was sent (§7.1).
    pub fn new(panel_width: u32, panel_height: u32, now: Instant) -> Self {
        Core {
            state: State::HelloSent,
            panel: (panel_width, panel_height),
            stream: None,
            last_stream_id: None,
            stream_start_deadline: now + STREAM_START_TIMEOUT,
            last_rx: now,
            last_keyframe_request: None,
            last_stats: now,
            stats_prev: (0, 0),
            decode_failures: 0,
            decoded_at_last_failure: 0,
            speed: SpeedTestReceiver::default(),
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The socket was closed outside the state machine (e.g. a failed write).
    pub fn on_closed(&mut self) {
        self.state = State::Closed;
    }

    pub fn on_message(&mut self, msg: Message, now: Instant) -> Vec<Command> {
        self.last_rx = now;
        if self.state == State::Closed {
            return vec![];
        }
        match msg {
            Message::Heartbeat { .. } | Message::Pong(_) => vec![],
            Message::Ping(p) => vec![Command::Send(Message::Pong(p))],
            Message::Goodbye { reason, message } => {
                log::info!("[net] peer said GOODBYE {reason}: {message:?}");
                self.state = State::Closed;
                vec![Command::Close]
            }
            Message::StreamStart(s) => self.stream_start(s, now),
            Message::StreamStop { stream_id } => self.stream_stop(stream_id),
            Message::SpeedTestStart(start) => {
                if let Err(e) = start.validate() {
                    return self.protocol_error(&e.to_string());
                }
                self.restart_stream_start_timer(now);
                self.speed
                    .start(start, now)
                    .map(|r| Command::Send(Message::SpeedTestResult(r)))
                    .into_iter()
                    .collect()
            }
            Message::SpeedTestEnd(end) => {
                self.restart_stream_start_timer(now);
                self.speed
                    .end(end.test_id, now)
                    .map(|r| Command::Send(Message::SpeedTestResult(r)))
                    .into_iter()
                    .collect()
            }
            Message::SpeedTestData { test_id, filler } => {
                self.speed.data(test_id, filler.len() as u64);
                vec![]
            }
            Message::Frame { header, .. } => {
                // `net` never produces this variant; treat it as a frame we
                // cannot push.
                self.on_frame(&header, now).1
            }
            Message::Unknown { msg_type, len } => {
                log::debug!("[net] skipped unknown message type {msg_type:#04x} ({len} bytes)");
                vec![]
            }
            m @ (Message::Hello(_)
            | Message::KeyframeRequest
            | Message::Stats(_)
            | Message::SpeedTestResult(_)) => {
                log::warn!("[net] ignoring receiver-bound {} from sender", m.name());
                vec![]
            }
        }
    }

    /// Filler bytes of one SPEED_TEST_DATA, which `net` never buffers.
    pub fn on_speed_test_data(&mut self, test_id: u32, filler_len: u64, now: Instant) {
        self.last_rx = now;
        self.speed.data(test_id, filler_len);
    }

    pub fn on_frame(&mut self, header: &FrameHeader, now: Instant) -> (FrameVerdict, Vec<Command>) {
        self.last_rx = now;
        match self.state {
            State::HelloSent => (
                FrameVerdict::Discard,
                self.protocol_error("FRAME before STREAM_START"),
            ),
            State::Paused | State::Closed => (FrameVerdict::Discard, vec![]),
            State::WaitKeyframe | State::Streaming => {
                let stream = self
                    .stream
                    .as_mut()
                    .expect("streaming states have a stream");
                stream.frames_received += 1;
                if self.state == State::WaitKeyframe {
                    if !header.is_keyframe() {
                        log::debug!("[net] discarding delta frame while waiting for a keyframe");
                        return (FrameVerdict::Discard, vec![]);
                    }
                    log::info!("[net] keyframe for stream {}: streaming", stream.stream_id);
                    self.state = State::Streaming;
                }
                // pts_us has an arbitrary epoch; rebase per stream (§6.2).
                let base = *stream.base_pts_us.get_or_insert(header.pts_us);
                let pts_ns = header.pts_us.saturating_sub(base).saturating_mul(1000);
                (FrameVerdict::Push { pts_ns }, vec![])
            }
        }
    }

    pub fn on_pipeline_event(
        &mut self,
        event: PipelineEvent,
        now: Instant,
        decoded_so_far: u64,
    ) -> Vec<Command> {
        if self.state == State::Closed {
            return vec![];
        }
        match event {
            PipelineEvent::DecodeWarning(text) => {
                log::warn!("[gst] decoder warning: {text}");
                self.request_keyframe(now).into_iter().collect()
            }
            PipelineEvent::DecodeError(text) => {
                // Any successful decode since the previous failure means the
                // failures are not consecutive.
                if decoded_so_far > self.decoded_at_last_failure {
                    self.decode_failures = 0;
                }
                self.decode_failures += 1;
                self.decoded_at_last_failure = decoded_so_far;
                log::error!(
                    "[gst] decode error {}/{MAX_DECODE_FAILURES}: {text}",
                    self.decode_failures
                );
                if self.decode_failures >= MAX_DECODE_FAILURES {
                    return self.goodbye(Reason::UNSUPPORTED_CODEC, "stream cannot be decoded");
                }
                let Some(stream) = self.stream.as_mut() else {
                    return vec![];
                };
                stream.wait_since = now;
                self.state = State::WaitKeyframe;
                let mut cmds = vec![Command::StartPipeline {
                    width: stream.width,
                    height: stream.height,
                    new_stream: false,
                }];
                cmds.extend(self.request_keyframe(now));
                cmds
            }
            PipelineEvent::Fatal(text) => {
                log::error!("[gst] pipeline failure: {text}");
                self.goodbye(Reason::INTERNAL_ERROR, &text)
            }
        }
    }

    /// Timers: liveness, STREAM_START timeout, keyframe wait, speed-test
    /// timeout and STATS. `counters` is `None` while no pipeline runs.
    pub fn on_tick(&mut self, now: Instant, counters: Option<CounterSnapshot>) -> Vec<Command> {
        if self.state == State::Closed {
            return vec![];
        }
        if now.saturating_duration_since(self.last_rx) >= LIVENESS_TIMEOUT {
            // §6.5: the peer is presumed gone, so no GOODBYE.
            log::warn!("[net] nothing received for {LIVENESS_TIMEOUT:?}; peer is dead");
            self.state = State::Closed;
            return vec![Command::Close];
        }

        let mut cmds = Vec::new();
        if let Some(result) = self.speed.poll_timeout(now) {
            self.restart_stream_start_timer(now);
            cmds.push(Command::Send(Message::SpeedTestResult(result)));
        }

        match self.state {
            State::HelloSent if !self.speed.is_active() && now >= self.stream_start_deadline => {
                cmds.extend(self.goodbye(Reason::TIMEOUT, "no STREAM_START within 5 s"));
            }
            State::WaitKeyframe => {
                let waiting = self.stream.as_ref().map(|s| s.wait_since);
                if waiting.is_some_and(|t| now.saturating_duration_since(t) >= KEYFRAME_WAIT) {
                    cmds.extend(self.request_keyframe(now));
                }
            }
            State::Streaming => {
                if let Some(c) = counters
                    && now.saturating_duration_since(self.last_stats) >= STATS_INTERVAL
                {
                    cmds.push(Command::Send(Message::Stats(self.stats(c))));
                    self.last_stats = now;
                }
            }
            _ => {}
        }
        cmds
    }

    fn stream_start(&mut self, s: StreamStart, now: Instant) -> Vec<Command> {
        if s.protocol_version != PROTOCOL_VERSION {
            return self.goodbye(
                Reason::VERSION_MISMATCH,
                &format!(
                    "receiver speaks v{PROTOCOL_VERSION}, sender v{}",
                    s.protocol_version
                ),
            );
        }
        if s.codec != CODEC_HEVC_MAIN {
            return self.goodbye(
                Reason::UNSUPPORTED_CODEC,
                &format!("codec {} not supported", s.codec),
            );
        }
        let (pw, ph) = self.panel;
        if s.width_px == 0 || s.height_px == 0 || s.width_px > pw || s.height_px > ph {
            return self.protocol_error(&format!(
                "STREAM_START {}x{} does not fit the {pw}x{ph} panel",
                s.width_px, s.height_px
            ));
        }
        if self.last_stream_id == Some(s.stream_id) {
            log::warn!("[net] STREAM_START reuses stream_id {}", s.stream_id);
        }
        log::info!(
            "[net] STREAM_START {}: {}x{} @ {}.{:03} fps{}",
            s.stream_id,
            s.width_px,
            s.height_px,
            s.fps_mhz / 1000,
            s.fps_mhz % 1000,
            if s.sflags & crate::protocol::SFLAG_HIDPI != 0 {
                " (hidpi)"
            } else {
                ""
            }
        );
        self.stream = Some(ActiveStream {
            stream_id: s.stream_id,
            width: s.width_px,
            height: s.height_px,
            wait_since: now,
            base_pts_us: None,
            frames_received: 0,
        });
        self.last_stream_id = Some(s.stream_id);
        self.state = State::WaitKeyframe;
        self.decode_failures = 0;
        self.decoded_at_last_failure = 0;
        self.last_stats = now;
        self.stats_prev = (0, 0);
        vec![Command::StartPipeline {
            width: s.width_px,
            height: s.height_px,
            new_stream: true,
        }]
    }

    fn stream_stop(&mut self, stream_id: u32) -> Vec<Command> {
        match self.state {
            State::HelloSent => self.protocol_error("STREAM_STOP before STREAM_START"),
            State::WaitKeyframe | State::Streaming => {
                let current = self.stream.as_ref().map(|s| s.stream_id);
                if current != Some(stream_id) {
                    log::warn!(
                        "[net] STREAM_STOP for {stream_id}, current is {current:?}; pausing anyway"
                    );
                }
                log::info!("[net] STREAM_STOP {stream_id}: paused");
                self.stream = None;
                self.state = State::Paused;
                vec![Command::StopPipeline]
            }
            State::Paused | State::Closed => vec![],
        }
    }

    fn stats(&mut self, c: CounterSnapshot) -> Stats {
        let (prev_ns, prev_samples) = self.stats_prev;
        let samples = c.decode_samples.saturating_sub(prev_samples);
        let avg_decode_us = c
            .decode_ns_total
            .saturating_sub(prev_ns)
            .checked_div(samples)
            .map_or(0, |ns| (ns / 1000).min(u32::MAX.into()) as u32);
        self.stats_prev = (c.decode_ns_total, c.decode_samples);
        let stream = self.stream.as_ref();
        Stats {
            stream_id: stream.map_or(0, |s| s.stream_id),
            frames_received: stream.map_or(0, |s| s.frames_received),
            frames_decoded: c.decoded,
            // Decoded but never reached the sink: the leaky queue replaced it.
            frames_dropped: c.decoded.saturating_sub(c.rendered),
            avg_decode_us,
            queue_depth: c.waiting.min(u32::MAX.into()) as u32,
        }
    }

    /// §6.4: at most one KEYFRAME_REQUEST per second.
    fn request_keyframe(&mut self, now: Instant) -> Option<Command> {
        let allowed = self
            .last_keyframe_request
            .is_none_or(|t| now.saturating_duration_since(t) >= KEYFRAME_REQUEST_MIN_INTERVAL);
        if !allowed {
            return None;
        }
        self.last_keyframe_request = Some(now);
        log::info!("[net] requesting keyframe");
        Some(Command::Send(Message::KeyframeRequest))
    }

    /// §6.9: the 5 s STREAM_START timer restarts when a speed test starts or
    /// ends; `on_tick` also holds it off while a test is active.
    fn restart_stream_start_timer(&mut self, now: Instant) {
        if self.state == State::HelloSent {
            self.stream_start_deadline = now + STREAM_START_TIMEOUT;
        }
    }

    fn protocol_error(&mut self, why: &str) -> Vec<Command> {
        log::warn!("[net] protocol error: {why}");
        self.goodbye(Reason::PROTOCOL_ERROR, why)
    }

    fn goodbye(&mut self, reason: Reason, why: &str) -> Vec<Command> {
        log::info!("[net] sending GOODBYE {reason}: {why}");
        self.state = State::Closed;
        vec![Command::Send(Message::goodbye(reason, why)), Command::Close]
    }
}

/// Everything the reader and ticker threads share, behind one mutex.
struct Live {
    core: Core,
    pipeline: Option<Pipeline>,
    counters: Arc<Counters>,
    cfg: PipelineConfig,
}

#[derive(Default)]
struct Outbox {
    messages: Vec<Message>,
    close: bool,
}

impl Live {
    /// Applies pipeline commands now; queues messages and Close for the
    /// caller to perform after releasing the lock.
    fn execute(&mut self, cmds: Vec<Command>, outbox: &mut Outbox) {
        let mut queue = VecDeque::from(cmds);
        while let Some(cmd) = queue.pop_front() {
            match cmd {
                Command::Send(m) => outbox.messages.push(m),
                Command::Close => {
                    // Stop now rather than after the GOODBYE write, which
                    // can block on a peer that stopped reading.
                    self.stop_pipeline();
                    outbox.close = true;
                }
                Command::StopPipeline => self.stop_pipeline(),
                Command::StartPipeline {
                    width,
                    height,
                    new_stream,
                } => {
                    self.pipeline = None;
                    if new_stream {
                        self.counters = Arc::new(Counters::default());
                    } else {
                        // AUs in the old appsrc/decoder will never decode.
                        self.counters.forget_inflight();
                    }
                    match Pipeline::new(width, height, &self.cfg, self.counters.clone()) {
                        Ok(p) => self.pipeline = Some(p),
                        Err(e) => queue.extend(self.core.on_pipeline_event(
                            PipelineEvent::Fatal(format!("{e:#}")),
                            Instant::now(),
                            0,
                        )),
                    }
                }
            }
        }
    }

    /// Dropping the pipeline sets it to NULL, which also makes a push the
    /// reader thread is blocked in return `Flushing`. Nothing else wakes it
    /// once a downstream error has stopped `appsrc`'s streaming task.
    fn stop_pipeline(&mut self) {
        self.pipeline = None;
    }
}

struct Shared {
    live: Mutex<Live>,
    writer: Mutex<FramedWriter<TcpStream>>,
    socket: TcpStream,
    closed: AtomicBool,
    slot: Arc<Slot>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Runs `f` under the lock, then sends what it produced.
    fn with_live<R>(&self, f: impl FnOnce(&mut Live) -> (R, Vec<Command>)) -> R {
        let mut outbox = Outbox::default();
        let result = {
            let mut live = lock(&self.live);
            let (result, cmds) = f(&mut live);
            live.execute(cmds, &mut outbox);
            result
        };
        self.flush(outbox);
        result
    }

    fn flush(&self, outbox: Outbox) {
        if !outbox.messages.is_empty() && !self.is_closed() {
            let mut writer = lock(&self.writer);
            for msg in &outbox.messages {
                if let Err(e) = writer.send(msg) {
                    log::info!("[net] write failed: {e}");
                    drop(writer);
                    self.close();
                    return;
                }
            }
        }
        if outbox.close {
            self.close();
        }
    }

    fn send(&self, msg: &Message) {
        self.flush(Outbox {
            messages: vec![msg.clone()],
            close: false,
        });
    }

    /// Never called with `live` held.
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            // Unblocks the reader thread's pending read.
            let _ = self.socket.shutdown(Shutdown::Both);
            // ...and its pending push, on every close path (including a
            // failed write), so the session can always end.
            {
                let mut live = lock(&self.live);
                live.core.on_closed();
                live.stop_pipeline();
            }
            self.slot.mark_closing();
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Serves one accepted connection, which already holds `slot`, until it
/// ends: sends HELLO, then runs the state machine. Returns once the socket is
/// closed, the pipeline stopped and `slot` released.
pub fn run(
    socket: TcpStream,
    peer: SocketAddr,
    hello: &Hello,
    cfg: &PipelineConfig,
    slot: Arc<Slot>,
) {
    if let Err(e) = serve(socket, hello, cfg, slot.clone()) {
        log::warn!("[net] {peer}: {e:#}");
    }
    slot.release();
    log::info!("[net] {peer}: session ended");
}

fn serve(
    socket: TcpStream,
    hello: &Hello,
    cfg: &PipelineConfig,
    slot: Arc<Slot>,
) -> anyhow::Result<()> {
    net::configure(&socket)?;
    let mut writer = FramedWriter::new(socket.try_clone()?);
    writer.send(&Message::Hello(hello.clone()))?;
    let shared = Arc::new(Shared {
        live: Mutex::new(Live {
            core: Core::new(hello.panel_width_px, hello.panel_height_px, Instant::now()),
            pipeline: None,
            counters: Arc::new(Counters::default()),
            cfg: cfg.clone(),
        }),
        writer: Mutex::new(writer),
        socket: socket.try_clone()?,
        closed: AtomicBool::new(false),
        slot,
    });

    let ticker = {
        let shared = shared.clone();
        thread::Builder::new()
            .name("idp-ticker".into())
            .spawn(move || tick_loop(&shared))?
    };
    read_loop(&shared, FramedReader::new(socket));
    shared.close();
    // Wake the ticker so the session releases the receiver without waiting
    // out a tick; a sender reconnecting right away should not see BUSY.
    ticker.thread().unpark();
    let _ = ticker.join();
    // Stop decoding and release the panel before listening again (§7.1).
    lock(&shared.live).pipeline = None;
    Ok(())
}

fn read_loop(shared: &Shared, mut reader: FramedReader<TcpStream>) {
    loop {
        let inbound = match reader.read() {
            Ok(inbound) => inbound,
            Err(e) => {
                if !shared.is_closed() {
                    match e {
                        NetError::Protocol(pe) => {
                            log::warn!("[net] protocol error: {pe}");
                            shared.send(&Message::goodbye(Reason::PROTOCOL_ERROR, pe.to_string()));
                        }
                        NetError::Closed => log::info!("[net] peer closed the connection"),
                        other => log::info!("[net] read failed: {other}"),
                    }
                }
                return;
            }
        };
        let now = Instant::now();
        match inbound {
            Inbound::Message(msg) => {
                log::trace!("[net] rx {}", msg.name());
                shared.with_live(|live| ((), live.core.on_message(msg, now)));
            }
            Inbound::SpeedTestData {
                test_id,
                filler_len,
            } => shared.with_live(|live| {
                live.core.on_speed_test_data(test_id, filler_len, now);
                ((), vec![])
            }),
            Inbound::Frame { header, au } => {
                let target = shared.with_live(|live| {
                    let (verdict, cmds) = live.core.on_frame(&header, now);
                    let target = match verdict {
                        FrameVerdict::Push { pts_ns } => {
                            live.pipeline.as_ref().map(|p| (p.frame_sink(), pts_ns))
                        }
                        FrameVerdict::Discard => None,
                    };
                    (target, cmds)
                });
                // Blocks while the decoder is behind: this is the TCP
                // backpressure of §8. Teardown unblocks it with Flushing.
                if let Some((sink, pts_ns)) = target
                    && let Err(e) = sink.push(au, pts_ns)
                {
                    log::debug!("[gst] push refused: {e:?}");
                }
            }
        }
        if shared.is_closed() {
            return;
        }
    }
}

fn tick_loop(shared: &Shared) {
    while !shared.is_closed() {
        thread::park_timeout(TICK);
        if shared.is_closed() {
            return;
        }
        let now = Instant::now();
        if lock(&shared.writer).idle_for(now) >= HEARTBEAT_IDLE {
            shared.send(&Message::Heartbeat {
                mono_us: net::mono_us(),
            });
        }
        shared.with_live(|live| {
            let mut events = Vec::new();
            if let Some(p) = &live.pipeline {
                while let Some(ev) = p.poll_event() {
                    events.push(ev);
                }
            }
            let mut cmds = Vec::new();
            for ev in events {
                let decoded = live.counters.snapshot().decoded;
                cmds.extend(live.core.on_pipeline_event(ev, now, decoded));
            }
            let counters = live.pipeline.as_ref().map(|_| live.counters.snapshot());
            cmds.extend(live.core.on_tick(now, counters));
            ((), cmds)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FFLAG_KEYFRAME, PingPong, SpeedTestEnd, SpeedTestStart};

    const MS: Duration = Duration::from_millis(1);

    fn start_msg(id: u32, w: u32, h: u32) -> Message {
        Message::StreamStart(StreamStart {
            protocol_version: 1,
            codec: 1,
            sflags: 0,
            width_px: w,
            height_px: h,
            fps_mhz: 60_000,
            stream_id: id,
        })
    }

    fn frame(pts_us: u64, key: bool) -> FrameHeader {
        FrameHeader {
            pts_us,
            fflags: if key { FFLAG_KEYFRAME } else { 0 },
        }
    }

    fn goodbye_reason(cmds: &[Command]) -> Option<Reason> {
        cmds.iter().find_map(|c| match c {
            Command::Send(Message::Goodbye { reason, .. }) => Some(*reason),
            _ => None,
        })
    }

    fn counts_keyframe_requests(cmds: &[Command]) -> usize {
        cmds.iter()
            .filter(|c| matches!(c, Command::Send(Message::KeyframeRequest)))
            .count()
    }

    #[test]
    fn happy_path_hello_start_keyframe_stream_stop() {
        let t0 = Instant::now();
        let mut core = Core::new(5120, 2880, t0);
        let cmds = core.on_message(start_msg(1, 5120, 2880), t0);
        assert_eq!(
            cmds,
            vec![Command::StartPipeline {
                width: 5120,
                height: 2880,
                new_stream: true
            }]
        );
        assert_eq!(core.state(), State::WaitKeyframe);

        assert_eq!(
            core.on_frame(&frame(10, false), t0).0,
            FrameVerdict::Discard
        );
        assert_eq!(
            core.on_frame(&frame(1_000, true), t0).0,
            FrameVerdict::Push { pts_ns: 0 }
        );
        assert_eq!(core.state(), State::Streaming);
        assert_eq!(
            core.on_frame(&frame(17_667, false), t0).0,
            FrameVerdict::Push { pts_ns: 16_667_000 }
        );

        assert_eq!(
            core.on_message(Message::StreamStop { stream_id: 1 }, t0),
            vec![Command::StopPipeline]
        );
        assert_eq!(core.state(), State::Paused);
        assert_eq!(
            core.on_frame(&frame(20_000, true), t0).0,
            FrameVerdict::Discard
        );

        // New geometry without reconnecting.
        let cmds = core.on_message(start_msg(2, 2560, 1440), t0);
        assert!(matches!(
            cmds[..],
            [Command::StartPipeline {
                width: 2560,
                new_stream: true,
                ..
            }]
        ));
        assert_eq!(core.state(), State::WaitKeyframe);
    }

    #[test]
    fn frame_before_stream_start_is_a_protocol_error() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        let (v, cmds) = core.on_frame(&frame(1, true), t0);
        assert_eq!(v, FrameVerdict::Discard);
        assert_eq!(goodbye_reason(&cmds), Some(Reason::PROTOCOL_ERROR));
        assert_eq!(cmds.last(), Some(&Command::Close));
        assert_eq!(core.state(), State::Closed);
    }

    #[test]
    fn stream_start_validation() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        assert_eq!(
            goodbye_reason(&core.on_message(start_msg(1, 3840, 2160), t0)),
            Some(Reason::PROTOCOL_ERROR)
        );

        let mut core = Core::new(1920, 1080, t0);
        let Message::StreamStart(mut s) = start_msg(1, 1920, 1080) else {
            unreachable!()
        };
        s.protocol_version = 2;
        assert_eq!(
            goodbye_reason(&core.on_message(Message::StreamStart(s), t0)),
            Some(Reason::VERSION_MISMATCH)
        );

        let mut core = Core::new(1920, 1080, t0);
        s.protocol_version = 1;
        s.codec = 2;
        assert_eq!(
            goodbye_reason(&core.on_message(Message::StreamStart(s), t0)),
            Some(Reason::UNSUPPORTED_CODEC)
        );
    }

    #[test]
    fn stream_start_timeout_and_speed_test_restart() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        let tick_at = |core: &mut Core, ms: u32| {
            let now = t0 + ms * MS;
            core.on_message(Message::Heartbeat { mono_us: 0 }, now);
            core.on_tick(now, None)
        };
        assert!(tick_at(&mut core, 4000).is_empty());

        // SPEED_TEST_START at 4 s restarts the timer (deadline 9 s), and a
        // test still in progress holds the timeout off.
        let start = SpeedTestStart {
            test_id: 1,
            duration_ms: 1000,
        };
        core.on_message(Message::SpeedTestStart(start), t0 + 4000 * MS);
        assert!(tick_at(&mut core, 9500).is_empty());

        // SPEED_TEST_END at 9.6 s restarts it again: deadline 14.6 s.
        let cmds = core.on_message(
            Message::SpeedTestEnd(SpeedTestEnd {
                test_id: 1,
                bytes_sent: 0,
            }),
            t0 + 9600 * MS,
        );
        assert!(matches!(
            cmds[..],
            [Command::Send(Message::SpeedTestResult(_))]
        ));
        assert!(tick_at(&mut core, 14_500).is_empty());
        assert_eq!(
            goodbye_reason(&tick_at(&mut core, 14_600)),
            Some(Reason::TIMEOUT)
        );
    }

    #[test]
    fn liveness_timeout_closes_without_goodbye() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        assert!(core.on_tick(t0 + 1999 * MS, None).is_empty());
        assert_eq!(core.on_tick(t0 + 2000 * MS, None), vec![Command::Close]);
        assert_eq!(core.state(), State::Closed);
    }

    #[test]
    fn ping_is_answered_with_matching_pong() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        let p = PingPong { seq: 5, t_us: 99 };
        assert_eq!(
            core.on_message(Message::Ping(p), t0),
            vec![Command::Send(Message::Pong(p))]
        );
    }

    #[test]
    fn keyframe_requests_after_500ms_and_at_most_once_per_second() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        core.on_message(start_msg(1, 1920, 1080), t0);
        let mut sent = Vec::new();
        for ms in (0..=2600).step_by(50) {
            let now = t0 + ms * MS;
            core.on_message(Message::Heartbeat { mono_us: 0 }, now);
            if counts_keyframe_requests(&core.on_tick(now, None)) > 0 {
                sent.push(ms);
            }
        }
        assert_eq!(sent, vec![500, 1500, 2500]);
    }

    #[test]
    fn decode_errors_restart_then_give_up_after_three() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        core.on_message(start_msg(1, 1280, 720), t0);
        core.on_frame(&frame(0, true), t0);
        let err = || PipelineEvent::DecodeError("boom".into());

        let cmds = core.on_pipeline_event(err(), t0, 0);
        assert!(cmds.contains(&Command::StartPipeline {
            width: 1280,
            height: 720,
            new_stream: false
        }));
        assert_eq!(counts_keyframe_requests(&cmds), 1);
        assert_eq!(core.state(), State::WaitKeyframe);

        // A successful decode in between resets the count.
        core.on_pipeline_event(err(), t0 + 1100 * MS, 10);
        core.on_pipeline_event(err(), t0 + 2200 * MS, 10);
        let cmds = core.on_pipeline_event(err(), t0 + 3300 * MS, 10);
        assert_eq!(goodbye_reason(&cmds), Some(Reason::UNSUPPORTED_CODEC));
    }

    #[test]
    fn stats_once_per_second_while_streaming() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        core.on_message(start_msg(7, 1920, 1080), t0);
        for i in 0..5 {
            core.on_frame(&frame(i * 16_667, i == 0), t0);
        }
        let snap = CounterSnapshot {
            pushed: 5,
            waiting: 1,
            decoded: 4,
            rendered: 3,
            decode_ns_total: 8_000_000,
            decode_samples: 4,
        };
        assert!(
            core.on_tick(t0 + 999 * MS, Some(snap))
                .iter()
                .all(|c| !matches!(c, Command::Send(Message::Stats(_))))
        );
        core.on_message(Message::Heartbeat { mono_us: 0 }, t0 + 1000 * MS);
        let cmds = core.on_tick(t0 + 1000 * MS, Some(snap));
        assert_eq!(
            cmds,
            vec![Command::Send(Message::Stats(Stats {
                stream_id: 7,
                frames_received: 5,
                frames_decoded: 4,
                frames_dropped: 1,
                avg_decode_us: 2000,
                queue_depth: 1,
            }))]
        );
    }

    #[test]
    fn speed_test_start_out_of_range_is_protocol_error() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        let cmds = core.on_message(
            Message::SpeedTestStart(SpeedTestStart {
                test_id: 1,
                duration_ms: 50,
            }),
            t0,
        );
        assert_eq!(goodbye_reason(&cmds), Some(Reason::PROTOCOL_ERROR));
    }

    #[test]
    fn goodbye_from_peer_closes_silently() {
        let t0 = Instant::now();
        let mut core = Core::new(1920, 1080, t0);
        assert_eq!(
            core.on_message(Message::goodbye(Reason::NORMAL, "bye"), t0),
            vec![Command::Close]
        );
        assert!(
            core.on_tick(t0 + 10 * STREAM_START_TIMEOUT, None)
                .is_empty()
        );
    }
}
