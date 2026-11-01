//! Panel probe (PLAN R3): native mode and physical size from DRM sysfs, the
//! receiver's stable `display_id` (PROTOCOL.md §6.1.1) and its name.

use std::path::Path;

use sha2::{Digest, Sha256};

pub const DEFAULT_DRM_DIR: &str = "/sys/class/drm";
pub const DEFAULT_REFRESH_MHZ: u32 = 60_000;

/// Used when no connected connector is found (e.g. running tests on a Mac).
const FALLBACK: PanelInfo = PanelInfo {
    connector: String::new(),
    width_px: 1920,
    height_px: 1080,
    refresh_mhz: DEFAULT_REFRESH_MHZ,
    width_mm: 0,
    height_mm: 0,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelInfo {
    /// DRM connector name such as `eDP-1`; empty when unknown.
    pub connector: String,
    pub width_px: u32,
    pub height_px: u32,
    pub refresh_mhz: u32,
    pub width_mm: u16,
    pub height_mm: u16,
}

/// `PANEL_WIDTH`, `PANEL_HEIGHT`, `PANEL_REFRESH_MHZ`, `PANEL_MM` (`597x336`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Overrides {
    pub width_px: Option<u32>,
    pub height_px: Option<u32>,
    pub refresh_mhz: Option<u32>,
    pub size_mm: Option<(u16, u16)>,
}

impl Overrides {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        fn parsed<T: std::str::FromStr>(key: &str, value: Option<String>) -> Option<T> {
            let value = value?;
            let parsed = value.trim().parse().ok();
            if parsed.is_none() {
                log::warn!("[display] ignoring unparsable {key}={value:?}");
            }
            parsed
        }
        let size_mm = get("PANEL_MM").and_then(|v| {
            let parsed =
                parse_wxh(&v).and_then(|(w, h)| Some((w.try_into().ok()?, h.try_into().ok()?)));
            if parsed.is_none() {
                log::warn!("[display] ignoring unparsable PANEL_MM={v:?} (want e.g. 597x336)");
            }
            parsed
        });
        Overrides {
            width_px: parsed("PANEL_WIDTH", get("PANEL_WIDTH")),
            height_px: parsed("PANEL_HEIGHT", get("PANEL_HEIGHT")),
            refresh_mhz: parsed("PANEL_REFRESH_MHZ", get("PANEL_REFRESH_MHZ")),
            size_mm,
        }
    }

    fn apply(&self, panel: &mut PanelInfo) {
        if let Some(v) = self.width_px {
            panel.width_px = v;
        }
        if let Some(v) = self.height_px {
            panel.height_px = v;
        }
        if let Some(v) = self.refresh_mhz {
            panel.refresh_mhz = v;
        }
        if let Some((w, h)) = self.size_mm {
            panel.width_mm = w;
            panel.height_mm = h;
        }
    }
}

/// Probes `drm_dir`, then applies overrides; never fails.
pub fn detect(drm_dir: &Path, overrides: &Overrides) -> PanelInfo {
    let mut panel = match probe(drm_dir) {
        Some(p) => p,
        None => {
            log::warn!(
                "[display] no connected DRM connector under {}; using fallback {}x{}",
                drm_dir.display(),
                FALLBACK.width_px,
                FALLBACK.height_px
            );
            FALLBACK
        }
    };
    overrides.apply(&mut panel);
    log::info!(
        "[display] panel {} {}x{} @ {}.{:03} Hz, {}x{} mm",
        if panel.connector.is_empty() {
            "?"
        } else {
            &panel.connector
        },
        panel.width_px,
        panel.height_px,
        panel.refresh_mhz / 1000,
        panel.refresh_mhz % 1000,
        panel.width_mm,
        panel.height_mm
    );
    panel
}

/// Finds the connected connector (internal panels first) and reads its
/// native mode and EDID. `None` if nothing usable is connected.
pub fn probe(drm_dir: &Path) -> Option<PanelInfo> {
    let mut connectors: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(drm_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file_name = e.file_name().into_string().ok()?;
            // Entries are `cardN` (the device) and `cardN-<connector>`.
            let (card, connector) = file_name.split_once('-')?;
            card.starts_with("card")
                .then(|| (connector.to_owned(), e.path()))
        })
        .filter(|(_, path)| read_trimmed(&path.join("status")).as_deref() == Some("connected"))
        .collect();
    connectors.sort_by_key(|(name, _)| (!is_internal(name), name.clone()));

    connectors.into_iter().find_map(|(connector, path)| {
        let modes = std::fs::read_to_string(path.join("modes")).ok()?;
        let (width_px, height_px) = modes.lines().find_map(parse_wxh)?;
        let edid = std::fs::read(path.join("edid"))
            .ok()
            .and_then(|b| parse_edid(&b));
        let (width_mm, height_mm) = edid.as_ref().map_or((0, 0), |e| (e.width_mm, e.height_mm));
        // sysfs `modes` has no refresh; trust the EDID's preferred timing only
        // when it describes the native mode (5K panels exceed the DTD's
        // pixel-clock range and advertise a lower mode there).
        let refresh_mhz = edid
            .and_then(|e| e.preferred)
            .filter(|t| (t.width_px, t.height_px) == (width_px, height_px))
            .map_or(DEFAULT_REFRESH_MHZ, |t| t.refresh_mhz);
        Some(PanelInfo {
            connector,
            width_px,
            height_px,
            refresh_mhz,
            width_mm,
            height_mm,
        })
    })
}

fn is_internal(connector: &str) -> bool {
    ["eDP", "LVDS", "DSI"]
        .iter()
        .any(|p| connector.starts_with(p))
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Parses `5120x2880` (sysfs modes may carry a suffix such as `i`).
fn parse_wxh(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once('x')?;
    let h: String = h.chars().take_while(char::is_ascii_digit).collect();
    Some((w.parse().ok()?, h.parse().ok()?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetailedTiming {
    pub width_px: u32,
    pub height_px: u32,
    pub refresh_mhz: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdidInfo {
    pub width_mm: u16,
    pub height_mm: u16,
    pub preferred: Option<DetailedTiming>,
}

/// Reads physical size (preferring the first detailed timing descriptor's
/// millimetres over the base block's centimetres) and the preferred timing.
pub fn parse_edid(bytes: &[u8]) -> Option<EdidInfo> {
    const MAGIC: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    let base = bytes.get(..128)?;
    if base[..8] != MAGIC || base.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
        log::warn!("[display] EDID has a bad header or checksum; ignoring it");
        return None;
    }
    let dtd = &base[54..72];
    let pixel_clock_10khz = u16::from_le_bytes([dtd[0], dtd[1]]);
    let preferred = (pixel_clock_10khz != 0).then(|| {
        let h_active = u32::from(dtd[2]) | (u32::from(dtd[4] >> 4) << 8);
        let h_blank = u32::from(dtd[3]) | (u32::from(dtd[4] & 0x0F) << 8);
        let v_active = u32::from(dtd[5]) | (u32::from(dtd[7] >> 4) << 8);
        let v_blank = u32::from(dtd[6]) | (u32::from(dtd[7] & 0x0F) << 8);
        let total = u64::from(h_active + h_blank) * u64::from(v_active + v_blank);
        let refresh_mhz = (u64::from(pixel_clock_10khz) * 10_000 * 1000)
            .checked_div(total)
            .unwrap_or(0) as u32;
        let size_mm = (
            u16::from(dtd[12]) | (u16::from(dtd[14] >> 4) << 8),
            u16::from(dtd[13]) | (u16::from(dtd[14] & 0x0F) << 8),
        );
        (
            DetailedTiming {
                width_px: h_active,
                height_px: v_active,
                refresh_mhz,
            },
            size_mm,
        )
    });
    let (width_mm, height_mm) = match preferred {
        Some((_, (w, h))) if w > 0 && h > 0 => (w, h),
        _ => (u16::from(base[21]) * 10, u16::from(base[22]) * 10),
    };
    Some(EdidInfo {
        width_mm,
        height_mm,
        preferred: preferred.map(|(t, _)| t),
    })
}

/// First 8 bytes of SHA-256(machine-id ‖ connector), big-endian (§6.1.1).
pub fn display_id(machine_id: &str, connector: &str) -> u64 {
    let digest = Sha256::new()
        .chain_update(machine_id.trim().as_bytes())
        .chain_update(connector.as_bytes())
        .finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("digest is 32 bytes"))
}

/// `/etc/machine-id` (or the D-Bus copy). Falls back to the hostname so the
/// id is at least stable on systems without one.
pub fn machine_id() -> String {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|p| read_trimmed(Path::new(p)).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| {
            log::warn!("[display] no machine-id found; deriving display_id from the hostname");
            hostname()
        })
}

pub fn hostname() -> String {
    ["/etc/hostname", "/proc/sys/kernel/hostname"]
        .iter()
        .find_map(|p| read_trimmed(Path::new(p)).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "imac-display".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snapshot() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/imac5k-drm")
    }

    #[test]
    fn probes_imac_5k_snapshot() {
        let panel = probe(&snapshot()).expect("eDP-1 is connected");
        assert_eq!(
            panel,
            PanelInfo {
                connector: "eDP-1".into(),
                width_px: 5120,
                height_px: 2880,
                refresh_mhz: 60_000,
                width_mm: 597,
                height_mm: 336,
            }
        );
    }

    #[test]
    fn parses_snapshot_edid() {
        let bytes = std::fs::read(snapshot().join("card0-eDP-1/edid")).unwrap();
        let edid = parse_edid(&bytes).unwrap();
        assert_eq!((edid.width_mm, edid.height_mm), (597, 336));
        let t = edid.preferred.unwrap();
        assert_eq!((t.width_px, t.height_px), (2560, 1440));
        assert_eq!(t.refresh_mhz, 59_950);
    }

    #[test]
    fn edid_falls_back_to_centimetres_and_rejects_bad_checksum() {
        let mut bytes = std::fs::read(snapshot().join("card0-eDP-1/edid")).unwrap();
        // Zero the DTD image size and fix up the checksum.
        bytes[54 + 12] = 0;
        bytes[54 + 13] = 0;
        bytes[54 + 14] = 0;
        bytes[127] = 0;
        bytes[127] = 0u8.wrapping_sub(bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
        let edid = parse_edid(&bytes).unwrap();
        assert_eq!((edid.width_mm, edid.height_mm), (600, 340));

        bytes[127] ^= 1;
        assert!(parse_edid(&bytes).is_none());
        assert!(parse_edid(&bytes[..100]).is_none());
    }

    #[test]
    fn missing_drm_dir_uses_fallback_and_overrides() {
        let missing = Path::new("/nonexistent/drm");
        assert!(probe(missing).is_none());
        assert_eq!(detect(missing, &Overrides::default()), FALLBACK);

        let overrides = Overrides::from_lookup(|k| {
            match k {
                "PANEL_WIDTH" => Some("2560"),
                "PANEL_HEIGHT" => Some("1440"),
                "PANEL_REFRESH_MHZ" => Some("59951"),
                "PANEL_MM" => Some("597x336"),
                _ => None,
            }
            .map(String::from)
        });
        let panel = detect(missing, &overrides);
        assert_eq!(
            (panel.width_px, panel.height_px, panel.refresh_mhz),
            (2560, 1440, 59_951)
        );
        assert_eq!((panel.width_mm, panel.height_mm), (597, 336));
    }

    #[test]
    fn unparsable_overrides_are_ignored() {
        let overrides = Overrides::from_lookup(|k| match k {
            "PANEL_WIDTH" => Some("wide".into()),
            "PANEL_MM" => Some("597".into()),
            _ => None,
        });
        assert_eq!(overrides, Overrides::default());
    }

    #[test]
    fn display_id_is_stable_and_connector_specific() {
        let machine_id =
            std::fs::read_to_string(snapshot().with_file_name("imac5k-machine-id")).unwrap();
        let id = display_id(&machine_id, "eDP-1");
        assert_eq!(id, display_id(machine_id.trim(), "eDP-1"));
        assert_ne!(id, display_id(&machine_id, "DP-1"));
        // Golden value: changing it changes every sender's remembered
        // arrangement for this receiver.
        assert_eq!(id, GOLDEN_SNAPSHOT_DISPLAY_ID, "{id:#018x}");
    }

    const GOLDEN_SNAPSHOT_DISPLAY_ID: u64 = 0xed68_45a2_8be8_eb74;

    #[test]
    fn parses_mode_lines() {
        assert_eq!(parse_wxh("5120x2880"), Some((5120, 2880)));
        assert_eq!(parse_wxh("1920x1080i\n"), Some((1920, 1080)));
        assert_eq!(parse_wxh("garbage"), None);
    }
}
