//! GStreamer decode pipeline (PLAN R2):
//! `appsrc → h265parse → decoder → [videoconvert] → queue(leaky, 1) → sink`.
//!
//! `appsrc` blocks when full, so a slow decoder stalls the socket reader and
//! TCP backpressure reaches the sender (PROTOCOL.md §8). Only *decoded* frames
//! are ever dropped, by the leaky queue in front of the sink.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use gst::prelude::*;

/// Hardware decoders first; `avdec_h265` (libav) is the software fallback.
const DECODER_PREFERENCE: [&str; 3] = ["vah265dec", "vaapih265dec", "avdec_h265"];
const SOFTWARE_DECODERS: [&str; 2] = ["avdec_h265", "libde265dec"];
/// Encoded AUs `appsrc` holds before `push` blocks.
const APPSRC_MAX_BUFFERS: u64 = 4;
const INFLIGHT_LIMIT: usize = 64;

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub sink: String,
    /// Resolved decoder element name (see `select_decoder`).
    pub decoder: String,
    /// Native panel mode, as advertised in HELLO.
    pub panel: (u32, u32),
}

/// Resolves `auto` to the best installed decoder, or checks a named one.
pub fn select_decoder(requested: &str) -> Result<String> {
    if requested != "auto" {
        if gst::ElementFactory::find(requested).is_none() {
            bail!("decoder element '{requested}' is not installed");
        }
        return Ok(requested.to_owned());
    }
    DECODER_PREFERENCE
        .iter()
        .find(|name| gst::ElementFactory::find(name).is_some())
        .map(|name| name.to_string())
        .context("no HEVC decoder found (install gstreamer1.0-plugins-bad with VA, or gstreamer1.0-libav)")
}

pub fn is_software_decoder(name: &str) -> bool {
    SOFTWARE_DECODERS.contains(&name)
}

/// Fails early, at startup, if the configured elements cannot be created.
pub fn check_elements(cfg: &PipelineConfig) -> Result<()> {
    for name in [
        "appsrc",
        "h265parse",
        "queue",
        cfg.decoder.as_str(),
        cfg.sink.as_str(),
    ] {
        if gst::ElementFactory::find(name).is_none() {
            bail!("GStreamer element '{name}' is not installed");
        }
    }
    if is_software_decoder(&cfg.decoder) && gst::ElementFactory::find("videoconvert").is_none() {
        bail!("GStreamer element 'videoconvert' is not installed");
    }
    Ok(())
}

/// HELLO caps bit 0: only `kmssink` scales a smaller stream to the panel.
pub fn sink_can_scale(sink: &str) -> bool {
    sink == "kmssink"
        && gst::ElementFactory::make(sink)
            .build()
            .is_ok_and(|e| e.has_property("can-scale"))
}

/// Frame counters for STATS (§6.7). Shared by every pipeline instance of one
/// stream, so a decoder restart does not reset them.
#[derive(Debug, Default)]
pub struct Counters {
    pushed: AtomicU64,
    decoded: AtomicU64,
    rendered: AtomicU64,
    decode_ns_total: AtomicU64,
    decode_samples: AtomicU64,
    /// (pts_ns, push time) of AUs not yet seen leaving the decoder.
    inflight: Mutex<VecDeque<(u64, Instant)>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterSnapshot {
    pub pushed: u64,
    /// Pushed AUs not yet out of the decoder (STATS `queue_depth`).
    pub waiting: u64,
    pub decoded: u64,
    pub rendered: u64,
    pub decode_ns_total: u64,
    pub decode_samples: u64,
}

impl Counters {
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            pushed: self.pushed.load(Ordering::Relaxed),
            waiting: self.lock_inflight().len() as u64,
            decoded: self.decoded.load(Ordering::Relaxed),
            rendered: self.rendered.load(Ordering::Relaxed),
            decode_ns_total: self.decode_ns_total.load(Ordering::Relaxed),
            decode_samples: self.decode_samples.load(Ordering::Relaxed),
        }
    }

    /// The pipeline was rebuilt: AUs still queued in the old one are gone.
    pub fn forget_inflight(&self) {
        self.lock_inflight().clear();
    }

    fn lock_inflight(&self) -> MutexGuard<'_, VecDeque<(u64, Instant)>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn on_pushed(&self, pts_ns: u64, now: Instant) {
        self.pushed.fetch_add(1, Ordering::Relaxed);
        let mut inflight = self.lock_inflight();
        if inflight.len() == INFLIGHT_LIMIT {
            inflight.pop_front();
        }
        inflight.push_back((pts_ns, now));
    }

    fn on_decoded(&self, pts_ns: Option<u64>) {
        self.decoded.fetch_add(1, Ordering::Relaxed);
        let Some(pts_ns) = pts_ns else { return };
        let mut inflight = self.lock_inflight();
        // Decode order == presentation order (§6.2), so anything older than
        // this PTS was consumed without output and can be forgotten.
        while let Some(&(pts, pushed_at)) = inflight.front() {
            if pts > pts_ns {
                break;
            }
            inflight.pop_front();
            if pts == pts_ns {
                let ns = pushed_at.elapsed().as_nanos() as u64;
                self.decode_ns_total.fetch_add(ns, Ordering::Relaxed);
                self.decode_samples.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineEvent {
    /// The parser or decoder failed; the pipeline must be rebuilt.
    DecodeError(String),
    /// The decoder reported a recoverable problem (e.g. a corrupt slice).
    DecodeWarning(String),
    /// Any other element failed (sink, allocation, ...).
    Fatal(String),
}

/// Pushes encoded access units into a running pipeline.
#[derive(Clone)]
pub struct FrameSink {
    appsrc: gst_app::AppSrc,
    counters: Arc<Counters>,
}

impl FrameSink {
    /// Blocks while `appsrc` is full. Returns `Err(Flushing)` once the
    /// pipeline has been stopped, which unblocks a pending push.
    pub fn push(&self, mut au: gst::Buffer, pts_ns: u64) -> Result<(), gst::FlowError> {
        au.get_mut()
            .expect("AU buffer is uniquely owned")
            .set_pts(gst::ClockTime::from_nseconds(pts_ns));
        self.counters.on_pushed(pts_ns, Instant::now());
        self.appsrc.push_buffer(au).map(|_| ())
    }
}

pub struct Pipeline {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    bus: gst::Bus,
    counters: Arc<Counters>,
    /// An ERROR was already reported; this instance only awaits teardown.
    failed: AtomicBool,
}

impl Pipeline {
    pub fn new(
        width: u32,
        height: u32,
        cfg: &PipelineConfig,
        counters: Arc<Counters>,
    ) -> Result<Self> {
        let caps = gst::Caps::builder("video/x-h265")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let appsrc = gst_app::AppSrc::builder()
            .name("src")
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .block(true)
            .build();
        if appsrc.has_property("max-buffers") {
            appsrc.set_property("max-buffers", APPSRC_MAX_BUFFERS);
            appsrc.set_property("max-bytes", 0u64);
        } else {
            appsrc.set_property("max-bytes", 16u64 * 1024 * 1024);
        }

        let parse = make("h265parse", "parse")?;
        let decoder = make(&cfg.decoder, "dec")?;
        let software = is_software_decoder(&cfg.decoder);
        if software && decoder.has_property("thread-type") {
            // Frame threading adds a frame of latency per thread.
            decoder.set_property_from_str("thread-type", "slice");
        }
        let queue = make("queue", "queue")?;
        queue.set_property("max-size-buffers", 1u32);
        queue.set_property("max-size-bytes", 0u32);
        queue.set_property("max-size-time", 0u64);
        queue.set_property_from_str("leaky", "downstream");
        let sink = make(&cfg.sink, "sink")?;
        configure_sink(&sink, (width, height) == cfg.panel);

        let mut chain: Vec<gst::Element> = vec![appsrc.clone().upcast(), parse, decoder.clone()];
        // The VA path stays in GPU memory (DMABuf/VAMemory) all the way to
        // kmssink; only software output needs a format conversion.
        if software {
            chain.push(make("videoconvert", "convert")?);
        }
        chain.extend([queue, sink.clone()]);

        let pipeline = gst::Pipeline::with_name("idp-receiver");
        pipeline.add_many(&chain)?;
        gst::Element::link_many(&chain)?;

        add_counting_probes(&decoder, &sink, &counters, width, height);

        let bus = pipeline.bus().context("pipeline has no bus")?;
        if let Err(e) = pipeline.set_state(gst::State::Playing) {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("pipeline failed to start: {e}");
        }
        log::info!(
            "[gst] pipeline up: {width}x{height}, decoder {} ({}), sink {}",
            cfg.decoder,
            if software { "software" } else { "hardware" },
            cfg.sink
        );
        Ok(Pipeline {
            pipeline,
            appsrc,
            bus,
            counters,
            failed: AtomicBool::new(false),
        })
    }

    pub fn frame_sink(&self) -> FrameSink {
        FrameSink {
            appsrc: self.appsrc.clone(),
            counters: self.counters.clone(),
        }
    }

    /// Next error or decoder warning from the bus, without blocking.
    ///
    /// Reports at most one error per pipeline instance. A failing element
    /// returns a flow error upstream, so `appsrc` (which owns the streaming
    /// thread) follows with its own "Internal data stream error"; that and
    /// anything after the first error are consequences, not new failures.
    pub fn poll_event(&self) -> Option<PipelineEvent> {
        use gst::MessageView;
        while let Some(msg) = self
            .bus
            .pop_filtered(&[gst::MessageType::Error, gst::MessageType::Warning])
        {
            let src = msg.src().map(|s| s.name().to_string()).unwrap_or_default();
            let (is_error, text) = match msg.view() {
                MessageView::Error(e) => (true, format!("{src}: {} ({:?})", e.error(), e.debug())),
                MessageView::Warning(w) => {
                    (false, format!("{src}: {} ({:?})", w.error(), w.debug()))
                }
                _ => continue,
            };
            if self.failed.load(Ordering::Relaxed) {
                log::debug!("[gst] ignored after the first error: {text}");
                continue;
            }
            let from_decoder = matches!(src.as_str(), "parse" | "dec");
            if is_error {
                self.failed.store(true, Ordering::Relaxed);
                return Some(if from_decoder {
                    PipelineEvent::DecodeError(text)
                } else {
                    PipelineEvent::Fatal(text)
                });
            }
            if from_decoder {
                return Some(PipelineEvent::DecodeWarning(text));
            }
            log::warn!("[gst] {text}");
        }
        None
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // NULL releases the sink (kmssink gives up the CRTC) and unblocks a
        // reader thread stuck in `appsrc.push_buffer`.
        let _ = self.pipeline.set_state(gst::State::Null);
        log::info!("[gst] pipeline stopped");
    }
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("cannot create GStreamer element '{factory}'"))
}

/// `native`: the stream has exactly the panel's native geometry.
fn configure_sink(sink: &gst::Element, native: bool) {
    if sink.has_property("sync") {
        sink.set_property("sync", false);
    }
    // kmssink: modesetting picks the connector mode whose size equals the
    // *stream* and fails if there is none, so only force it (to take the
    // CRTC even if something configured it before us) for a native-size
    // stream. A smaller one keeps the current native mode and is scaled up
    // by the plane (`can-scale`, on by default), as HELLO caps bit 0 says.
    if sink.has_property("force-modesetting") {
        sink.set_property("force-modesetting", native);
    }
    if sink.has_property("fullscreen") {
        sink.set_property("fullscreen", true);
    }
}

fn add_counting_probes(
    decoder: &gst::Element,
    sink: &gst::Element,
    counters: &Arc<Counters>,
    width: u32,
    height: u32,
) {
    if let Some(pad) = decoder.static_pad("src") {
        let counters = counters.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                counters.on_decoded(buffer.pts().map(|p| p.nseconds()));
            }
            gst::PadProbeReturn::Ok
        });
    }
    if let Some(pad) = sink.static_pad("sink") {
        let counters = counters.clone();
        let first = AtomicBool::new(true);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counters.rendered.fetch_add(1, Ordering::Relaxed);
            if first.swap(false, Ordering::Relaxed) {
                log::info!("[gst] first frame rendered ({width}x{height})");
            }
            gst::PadProbeReturn::Ok
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn decode_time_matches_pts_and_forgets_skipped_frames() {
        let c = Counters::default();
        let t0 = Instant::now() - Duration::from_millis(10);
        c.on_pushed(1, t0);
        c.on_pushed(2, t0);
        c.on_pushed(3, t0);
        c.on_decoded(Some(2));
        let s = c.snapshot();
        assert_eq!((s.pushed, s.decoded, s.decode_samples), (3, 1, 1));
        assert!(s.decode_ns_total >= 10_000_000);
        assert_eq!(s.waiting, 1, "pts 1 consumed without output, 3 left");
        c.on_decoded(None);
        assert_eq!(c.snapshot().decoded, 2);
    }

    #[test]
    fn queue_depth_survives_frames_lost_in_a_restart() {
        let c = Counters::default();
        let t0 = Instant::now();
        for pts in 1..=5 {
            c.on_pushed(pts, t0);
        }
        c.forget_inflight();
        c.on_pushed(10, t0);
        c.on_pushed(11, t0);
        c.on_decoded(Some(10));
        let s = c.snapshot();
        assert_eq!((s.pushed, s.decoded, s.waiting), (7, 1, 1));
    }

    /// `frames` HEVC access units (Annex-B, AU-aligned) from x265enc.
    fn hevc_clip(frames: u32) -> Vec<gst::Buffer> {
        assert!(
            gst::ElementFactory::find("x265enc").is_some(),
            "x265enc is required"
        );
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers={frames} \
             ! video/x-raw,format=I420,width=320,height=180,framerate=60/1 \
             ! x265enc tune=zerolatency speed-preset=ultrafast key-int-max=100000 \
             ! h265parse config-interval=-1 \
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
            out.push(sample.buffer().unwrap().copy_deep().unwrap());
        }
        pipeline.set_state(gst::State::Null).unwrap();
        out
    }

    /// A real decoder error is followed by appsrc's own "Internal data
    /// stream error"; only the first may reach the session, as a
    /// DecodeError, or the §9 restart path would end in INTERNAL_ERROR.
    #[test]
    fn decoder_error_is_reported_once_as_a_decode_error() {
        gst::init().unwrap();
        let clip = hevc_clip(12);
        let cfg = PipelineConfig {
            sink: "fakesink".into(),
            decoder: "identity".into(),
            panel: (320, 180),
        };
        let p = Pipeline::new(320, 180, &cfg, Arc::new(Counters::default())).unwrap();
        // identity arms `error-after` when it starts, so restart it.
        p.pipeline.set_state(gst::State::Ready).unwrap();
        p.pipeline
            .by_name("dec")
            .unwrap()
            .set_property("error-after", 2i32);
        p.pipeline.set_state(gst::State::Playing).unwrap();

        let sink = p.frame_sink();
        let pusher = std::thread::spawn(move || {
            for (i, au) in clip.into_iter().enumerate() {
                // Blocks once the failed pipeline stops draining appsrc;
                // dropping the pipeline below releases it.
                if sink.push(au, i as u64 * 16_666_667).is_err() {
                    return;
                }
            }
        });

        let until = Instant::now() + Duration::from_secs(3);
        let mut events = Vec::new();
        while Instant::now() < until && events.is_empty() {
            events.extend(std::iter::from_fn(|| p.poll_event()));
            std::thread::sleep(Duration::from_millis(20));
        }
        // Give appsrc's follow-up error time to land on the bus.
        std::thread::sleep(Duration::from_millis(300));
        events.extend(std::iter::from_fn(|| p.poll_event()));
        drop(p);
        pusher.join().unwrap();

        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(&events[0], PipelineEvent::DecodeError(t) if t.starts_with("dec:")),
            "{events:?}"
        );
    }

    #[test]
    fn software_decoder_classification() {
        assert!(is_software_decoder("avdec_h265"));
        assert!(!is_software_decoder("vah265dec"));
    }

    #[test]
    fn named_decoder_must_exist() {
        gst::init().unwrap();
        assert!(select_decoder("no_such_decoder_element").is_err());
    }
}
