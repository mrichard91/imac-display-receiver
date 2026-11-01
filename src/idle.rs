//! Idle screen (PLAN R6): while no sender is connected, the panel shows the
//! receiver's name and the `address:port` of each network interface, so a
//! user at any Mac knows what to type in Connect….

use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;

/// Rendered at most at 1080p and scaled (or centred) by the sink: text only
/// needs to be legible, and 5K frames would cost far more to draw.
const MAX_HEIGHT: u32 = 1080;

pub struct IdleScreen {
    sink: String,
    width: u32,
    height: u32,
    name: String,
    port: u16,
    shown: Mutex<Option<Shown>>,
}

struct Shown {
    pipeline: gst::Pipeline,
    overlay: gst::Element,
    text: String,
}

impl Drop for Shown {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl IdleScreen {
    /// `None` (with a warning) if the elements it needs are not installed.
    pub fn new(
        sink: &str,
        panel_width: u32,
        panel_height: u32,
        name: &str,
        port: u16,
    ) -> Option<Self> {
        let missing: Vec<_> = ["videotestsrc", "textoverlay", "videoconvert"]
            .into_iter()
            .filter(|e| gst::ElementFactory::find(e).is_none())
            .collect();
        if !missing.is_empty() {
            log::warn!(
                "[display] idle screen disabled; missing {}",
                missing.join(", ")
            );
            return None;
        }
        let (width, height) = fit(panel_width, panel_height);
        Some(IdleScreen {
            sink: sink.to_owned(),
            width,
            height,
            name: name.to_owned(),
            port,
            shown: Mutex::new(None),
        })
    }

    pub fn show(&self) {
        let mut shown = self.lock();
        if shown.is_some() {
            return;
        }
        let text = idle_text(&self.name, self.port, &local_addresses());
        match self.build(&text) {
            Ok(s) => {
                log::info!("[display] idle screen up");
                log::debug!("[display] idle text: {text:?}");
                *shown = Some(s);
            }
            Err(e) => log::warn!("[display] idle screen failed: {e:#}"),
        }
    }

    pub fn hide(&self) {
        if self.lock().take().is_some() {
            log::info!("[display] idle screen down");
        }
    }

    /// Picks up address changes (DHCP, a Thunderbolt cable plugged in) and
    /// drops the screen if its pipeline failed.
    pub fn refresh(&self) {
        let mut shown = self.lock();
        let Some(s) = shown.as_mut() else { return };
        if let Some(bus) = s.pipeline.bus()
            && let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error])
            && let gst::MessageView::Error(e) = msg.view()
        {
            log::warn!("[display] idle screen stopped: {}", e.error());
            *shown = None;
            return;
        }
        let text = idle_text(&self.name, self.port, &local_addresses());
        if text != s.text {
            s.overlay.set_property("text", &text);
            s.text = text;
        }
    }

    fn build(&self, text: &str) -> Result<Shown> {
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc pattern=black is-live=true \
             ! video/x-raw,width={w},height={h},framerate=1/1 \
             ! textoverlay name=overlay valignment=center halignment=center \
               line-alignment=center font-desc=\"Sans 28\" shaded-background=false \
             ! videoconvert ! {sink} name=sink",
            w = self.width,
            h = self.height,
            sink = self.sink,
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("idle pipeline is not a pipeline"))?;
        let overlay = pipeline.by_name("overlay").context("no textoverlay")?;
        overlay.set_property("text", text);
        if let Some(sink) = pipeline.by_name("sink")
            && sink.has_property("sync")
        {
            sink.set_property("sync", false);
        }
        pipeline.set_state(gst::State::Playing)?;
        Ok(Shown {
            pipeline,
            overlay,
            text: text.to_owned(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<Shown>> {
        self.shown.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Scales the panel size down to at most `MAX_HEIGHT` lines, keeping the
/// aspect ratio and even dimensions.
fn fit(width: u32, height: u32) -> (u32, u32) {
    if height <= MAX_HEIGHT || height == 0 {
        return (width & !1, height & !1);
    }
    let w = (u64::from(width) * u64::from(MAX_HEIGHT) / u64::from(height)) as u32;
    (w & !1, MAX_HEIGHT)
}

pub fn idle_text(name: &str, port: u16, addrs: &[(String, IpAddr)]) -> String {
    let mut text = format!("{name}\n\n");
    if addrs.is_empty() {
        text.push_str("Waiting for a network connection…");
        return text;
    }
    text.push_str("Connect from your Mac to:\n");
    for (iface, ip) in addrs {
        let addr = match ip {
            IpAddr::V4(v4) => format!("{v4}:{port}"),
            IpAddr::V6(v6) => format!("[{v6}]:{port}"),
        };
        text.push_str(&format!("\n{addr}   ({iface})"));
    }
    text
}

/// Addresses a sender can type: no loopback, no IPv6 link-local (those need
/// a scope id that Connect… does not take).
pub fn local_addresses() -> Vec<(String, IpAddr)> {
    let mut out: Vec<(String, IpAddr)> = interface_addresses()
        .into_iter()
        .filter(|(_, ip)| match ip {
            IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
            IpAddr::V6(v6) => {
                !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local()
            }
        })
        .collect();
    // IPv4 first: shorter to type.
    out.sort_by_key(|(iface, ip)| (ip.is_ipv6(), iface.clone()));
    out
}

fn interface_addresses() -> Vec<(String, IpAddr)> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `ifap` with a linked list that stays valid
    // until freeifaddrs; every pointer is null-checked before use and each
    // sockaddr is read as the type its family field names.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while let Some(ifa) = cur.as_ref() {
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
                continue;
            }
            let ip = match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            out.push((name, ip));
        }
        libc::freeifaddrs(ifap);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_lists_each_address_with_port() {
        let addrs = vec![
            (
                "thunderbolt0".to_owned(),
                IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2)),
            ),
            ("enp3s0".to_owned(), "2001:db8::2".parse().unwrap()),
        ];
        let text = idle_text("iMac", 7878, &addrs);
        assert!(text.starts_with("iMac\n"));
        assert!(text.contains("10.99.0.2:7878   (thunderbolt0)"));
        assert!(text.contains("[2001:db8::2]:7878   (enp3s0)"));
        assert!(idle_text("iMac", 7878, &[]).contains("Waiting"));
    }

    #[test]
    fn local_addresses_exclude_loopback() {
        assert!(local_addresses().iter().all(|(_, ip)| !ip.is_loopback()));
    }

    #[test]
    fn idle_frame_size_is_capped_at_1080_lines() {
        assert_eq!(fit(5120, 2880), (1920, 1080));
        assert_eq!(fit(1280, 720), (1280, 720));
        assert_eq!(fit(2560, 1600), (1728, 1080));
    }
}
