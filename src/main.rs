use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use imac_display_rx::idle::IdleScreen;
use imac_display_rx::net::{self, FramedWriter};
use imac_display_rx::pipeline::{self, PipelineConfig};
use imac_display_rx::protocol::{
    CAPS_CAN_SCALE, CAPS_SPEED_TEST, CAPS_STATS, CODECS_HEVC_MAIN, Hello, Message,
    PROTOCOL_VERSION, Reason,
};
use imac_display_rx::slot::Slot;
use imac_display_rx::{panel, session};

/// How long a new sender may wait for a session that is already closing.
const HANDOVER_WAIT: Duration = Duration::from_secs(1);
const IDLE_REFRESH: Duration = Duration::from_secs(5);

/// iMac Display Protocol receiver: decodes the HEVC stream from a Mac sender
/// and shows it full screen on this machine's panel.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// TCP port to listen on (all interfaces). 0 picks a free port.
    #[arg(long, env = "PORT", default_value_t = 7878)]
    port: u16,
    /// GStreamer video sink (kmssink on the iMac; fakesink for tests).
    #[arg(long, env = "SINK", default_value = "kmssink")]
    sink: String,
    /// HEVC decoder element, or `auto` (vah265dec, vaapih265dec, avdec_h265).
    #[arg(long, env = "DECODER", default_value = "auto")]
    decoder: String,
    /// Log filter, e.g. `info` or `debug`.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,
    /// Name shown to senders; defaults to the hostname.
    #[arg(long, env = "RECEIVER_NAME")]
    name: Option<String>,
    /// Bitrate hint for senders in kbit/s; 0 = no preference.
    #[arg(long, env = "MAX_BITRATE_KBPS", default_value_t = 0)]
    max_bitrate_kbps: u32,
    /// DRM sysfs directory to probe for the panel.
    #[arg(long, env = "DRM_SYSFS", default_value = panel::DEFAULT_DRM_DIR)]
    drm_sysfs: PathBuf,
    /// Show the receiver's name and addresses on the panel while no sender
    /// is connected.
    #[arg(long, env = "IDLE_SCREEN", default_value_t = true, action = clap::ArgAction::Set)]
    idle_screen: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli.log_level);
    gst::init().context("GStreamer init failed")?;

    let decoder = pipeline::select_decoder(&cli.decoder)?;
    let hello = build_hello(&cli);
    let cfg = PipelineConfig {
        sink: cli.sink.clone(),
        decoder,
        panel: (hello.panel_width_px, hello.panel_height_px),
    };
    pipeline::check_elements(&cfg)?;
    log::info!("[gst] decoder {}, sink {}", cfg.decoder, cfg.sink);
    let listener = net::bind(cli.port).context("cannot bind the receiver port")?;
    let local = listener.local_addr()?;
    log::info!("[net] listening on {local} as {:?}", hello.name);

    let slot = Arc::new(match idle_screen(&cli, &hello, local.port()) {
        Some(idle) => {
            idle.show();
            let (taken, freed) = (idle.clone(), idle.clone());
            thread::spawn(move || {
                loop {
                    thread::sleep(IDLE_REFRESH);
                    idle.refresh();
                }
            });
            Slot::with_hooks(move || taken.hide(), move || freed.show())
        }
        None => Slot::default(),
    });
    for conn in listener.incoming() {
        let socket = match conn {
            Ok(s) => s,
            Err(e) => {
                log::warn!("[net] accept failed: {e}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let Ok(peer) = socket.peer_addr() else {
            continue;
        };
        let (hello, cfg, slot) = (hello.clone(), cfg.clone(), slot.clone());
        thread::Builder::new()
            .name("idp-session".into())
            .spawn(move || {
                if !slot.try_acquire(HANDOVER_WAIT) {
                    log::info!("[net] {peer}: rejecting, a sender is already connected");
                    reject_busy(socket);
                    return;
                }
                log::info!("[net] {peer}: connected");
                session::run(socket, peer, &hello, &cfg, slot);
                log::info!("[net] listening");
            })?;
    }
    Ok(())
}

fn idle_screen(cli: &Cli, hello: &Hello, port: u16) -> Option<Arc<IdleScreen>> {
    if !cli.idle_screen {
        return None;
    }
    IdleScreen::new(
        &cli.sink,
        hello.panel_width_px,
        hello.panel_height_px,
        &hello.name,
        port,
    )
    .map(Arc::new)
}

fn build_hello(cli: &Cli) -> Hello {
    let panel = panel::detect(&cli.drm_sysfs, &panel::Overrides::from_env());
    let display_id = panel::display_id(&panel::machine_id(), &panel.connector);
    let mut caps = CAPS_STATS | CAPS_SPEED_TEST;
    if pipeline::sink_can_scale(&cli.sink) {
        caps |= CAPS_CAN_SCALE;
    }
    Hello {
        protocol_version: PROTOCOL_VERSION,
        caps,
        panel_width_px: panel.width_px,
        panel_height_px: panel.height_px,
        refresh_mhz: panel.refresh_mhz,
        physical_width_mm: panel.width_mm,
        physical_height_mm: panel.height_mm,
        display_id,
        codecs: CODECS_HEVC_MAIN,
        max_bitrate_kbps: cli.max_bitrate_kbps,
        name: cli.name.clone().unwrap_or_else(panel::hostname),
    }
}

/// §7.1: a second sender gets GOODBYE BUSY and is closed.
fn reject_busy(socket: TcpStream) {
    let _ = net::configure(&socket);
    let mut writer = FramedWriter::new(&socket);
    let _ = writer.send(&Message::goodbye(Reason::BUSY, "busy"));
    let _ = socket.shutdown(std::net::Shutdown::Both);
}

fn init_logging(filter: &str) {
    env_logger::Builder::new()
        .parse_filters(filter)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} {:<5} {}",
                buf.timestamp_millis(),
                record.level(),
                record.args()
            )
        })
        .init();
}
