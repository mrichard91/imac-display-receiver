//! IDP v1 wire format (PROTOCOL.md §4–§6): message types, encode and decode.
//!
//! `Message` is the owned, fully decoded form of every message. The two bulk
//! types, FRAME and SPEED_TEST_DATA, also have fixed-size sub-headers
//! (`FrameHeader`, `SpeedTestDataHeader`) so `net.rs` can stream their bodies
//! (into a `gst::Buffer`, or into a scratch buffer to discard) without ever
//! materialising a `Message` for them.

use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 8;
pub const MAX_PAYLOAD_LEN: u32 = 64 * 1024 * 1024;

/// `codecs` bit 0 in HELLO, and the only STREAM_START `codec` value in v1.
pub const CODEC_HEVC_MAIN: u8 = 1;
pub const CODECS_HEVC_MAIN: u32 = 1;

pub const CAPS_CAN_SCALE: u16 = 1 << 0;
pub const CAPS_STATS: u16 = 1 << 1;
pub const CAPS_SPEED_TEST: u16 = 1 << 2;

pub const SFLAG_HIDPI: u8 = 1 << 0;
pub const FFLAG_KEYFRAME: u8 = 1 << 0;

pub const SPEED_TEST_OK: u32 = 0;
pub const SPEED_TEST_ABORTED: u32 = 1;
pub const SPEED_TEST_MIN_MS: u32 = 100;
pub const SPEED_TEST_MAX_MS: u32 = 10_000;

/// Message type codes (§5).
pub mod msg_type {
    pub const HELLO: u8 = 0x01;
    pub const KEYFRAME_REQUEST: u8 = 0x02;
    pub const HEARTBEAT: u8 = 0x03;
    pub const STATS: u8 = 0x04;
    pub const GOODBYE: u8 = 0x05;
    pub const PING: u8 = 0x06;
    pub const PONG: u8 = 0x07;
    pub const SPEED_TEST_RESULT: u8 = 0x08;
    pub const STREAM_START: u8 = 0x10;
    pub const FRAME: u8 = 0x11;
    pub const STREAM_STOP: u8 = 0x12;
    pub const SPEED_TEST_START: u8 = 0x13;
    pub const SPEED_TEST_DATA: u8 = 0x14;
    pub const SPEED_TEST_END: u8 = 0x15;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("payload length {0} exceeds 64 MiB")]
    TooLong(u32),
    #[error("message type {msg_type:#04x}: payload is {actual} bytes, expected {expected}")]
    WrongLength {
        msg_type: u8,
        expected: usize,
        actual: usize,
    },
    #[error("message type {msg_type:#04x}: payload shorter than {min} bytes")]
    Truncated { msg_type: u8, min: usize },
    #[error("string extends past the end of the payload")]
    StringOverrun,
    #[error("string is not valid UTF-8")]
    BadUtf8,
    #[error("{0}")]
    Invalid(&'static str),
}

/// GOODBYE reason codes (§6.6). Kept as a raw `u16` so unknown reasons from a
/// newer peer still round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reason(pub u16);

impl Reason {
    pub const NORMAL: Reason = Reason(0);
    pub const VERSION_MISMATCH: Reason = Reason(1);
    pub const BUSY: Reason = Reason(2);
    pub const UNSUPPORTED_CODEC: Reason = Reason(3);
    pub const TIMEOUT: Reason = Reason(4);
    pub const PROTOCOL_ERROR: Reason = Reason(5);
    pub const INTERNAL_ERROR: Reason = Reason(6);

    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "NORMAL",
            1 => "VERSION_MISMATCH",
            2 => "BUSY",
            3 => "UNSUPPORTED_CODEC",
            4 => "TIMEOUT",
            5 => "PROTOCOL_ERROR",
            6 => "INTERNAL_ERROR",
            _ => "UNKNOWN",
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name(), self.0)
    }
}

/// The 8-byte message header (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub msg_type: u8,
    pub hflags: u8,
    pub reserved: u16,
    pub length: u32,
}

impl Header {
    pub fn new(msg_type: u8, length: u32) -> Self {
        Header {
            msg_type,
            hflags: 0,
            reserved: 0,
            length,
        }
    }

    pub fn parse(bytes: &[u8; HEADER_LEN]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(bytes);
        let header = Header {
            msg_type: r.u8(),
            hflags: r.u8(),
            reserved: r.u16(),
            length: r.u32(),
        };
        if header.length > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::TooLong(header.length));
        }
        Ok(header)
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = self.msg_type;
        out[1] = self.hflags;
        out[2..4].copy_from_slice(&self.reserved.to_be_bytes());
        out[4..8].copy_from_slice(&self.length.to_be_bytes());
        out
    }

    pub fn has_reserved_bits(&self) -> bool {
        self.hflags != 0 || self.reserved != 0
    }
}

/// True when `payload` sets what §9 says to ignore but log: undefined flag
/// bits or non-zero reserved fields. For FRAME and SPEED_TEST_DATA the fixed
/// prefix is enough; the header's own fields are `Header::has_reserved_bits`.
pub fn payload_has_reserved_bits(msg_type: u8, payload: &[u8]) -> bool {
    let flags = |at: usize, defined: u8| payload.get(at).is_some_and(|f| f & !defined != 0);
    let nonzero = |range: std::ops::Range<usize>| {
        payload
            .get(range)
            .is_some_and(|bytes| bytes.iter().any(|&b| b != 0))
    };
    match msg_type {
        msg_type::FRAME => flags(8, FFLAG_KEYFRAME) || nonzero(9..12),
        msg_type::STREAM_START => flags(3, SFLAG_HIDPI),
        msg_type::SPEED_TEST_START => nonzero(8..12),
        msg_type::SPEED_TEST_DATA => nonzero(4..8),
        _ => false,
    }
}

/// The fixed 12-byte prefix of a FRAME payload (§6.2). The access unit follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub pts_us: u64,
    pub fflags: u8,
}

impl FrameHeader {
    pub const LEN: usize = 12;

    pub fn parse(bytes: &[u8; Self::LEN]) -> Self {
        let mut r = Reader::new(bytes);
        FrameHeader {
            pts_us: r.u64(),
            fflags: r.u8(),
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[..8].copy_from_slice(&self.pts_us.to_be_bytes());
        out[8] = self.fflags;
        out
    }

    pub fn is_keyframe(&self) -> bool {
        self.fflags & FFLAG_KEYFRAME != 0
    }

    /// Full message header for a FRAME carrying an access unit of `au_len` bytes.
    pub fn message_header(au_len: usize) -> Result<Header, ProtocolError> {
        let len = Self::LEN + au_len;
        let len = u32::try_from(len)
            .ok()
            .filter(|&l| l <= MAX_PAYLOAD_LEN)
            .ok_or(ProtocolError::TooLong(u32::MAX))?;
        Ok(Header::new(msg_type::FRAME, len))
    }
}

/// The fixed 8-byte prefix of a SPEED_TEST_DATA payload (§6.9). Filler follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedTestDataHeader {
    pub test_id: u32,
}

impl SpeedTestDataHeader {
    pub const LEN: usize = 8;

    pub fn parse(bytes: &[u8; Self::LEN]) -> Self {
        SpeedTestDataHeader {
            test_id: Reader::new(bytes).u32(),
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[..4].copy_from_slice(&self.test_id.to_be_bytes());
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub protocol_version: u16,
    pub caps: u16,
    pub panel_width_px: u32,
    pub panel_height_px: u32,
    pub refresh_mhz: u32,
    pub physical_width_mm: u16,
    pub physical_height_mm: u16,
    pub display_id: u64,
    pub codecs: u32,
    pub max_bitrate_kbps: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStart {
    pub protocol_version: u16,
    pub codec: u8,
    pub sflags: u8,
    pub width_px: u32,
    pub height_px: u32,
    pub fps_mhz: u32,
    pub stream_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub stream_id: u32,
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    pub avg_decode_us: u32,
    pub queue_depth: u32,
}

/// Payload of both PING and PONG (§6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingPong {
    pub seq: u32,
    pub t_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedTestStart {
    pub test_id: u32,
    pub duration_ms: u32,
}

impl SpeedTestStart {
    /// Value checks that §9 classifies as protocol errors.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.test_id == 0 {
            return Err(ProtocolError::Invalid("SPEED_TEST_START test_id is 0"));
        }
        if !(SPEED_TEST_MIN_MS..=SPEED_TEST_MAX_MS).contains(&self.duration_ms) {
            return Err(ProtocolError::Invalid(
                "SPEED_TEST_START duration_ms outside 100..=10000",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedTestEnd {
    pub test_id: u32,
    pub bytes_sent: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedTestResult {
    pub test_id: u32,
    pub status: u32,
    pub bytes_received: u64,
    pub elapsed_us: u64,
}

impl SpeedTestResult {
    /// Throughput per §6.9 step 5; `None` when nothing was timed.
    pub fn kbit_per_s(&self) -> Option<u64> {
        (self.elapsed_us > 0).then(|| self.bytes_received * 8 * 1000 / self.elapsed_us)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    KeyframeRequest,
    Heartbeat {
        mono_us: u64,
    },
    Stats(Stats),
    Goodbye {
        reason: Reason,
        message: String,
    },
    Ping(PingPong),
    Pong(PingPong),
    SpeedTestResult(SpeedTestResult),
    StreamStart(StreamStart),
    /// Owned form, used by tests and the test sender. The receiver's hot path
    /// reads `FrameHeader` and streams the AU instead (see `net.rs`).
    Frame {
        header: FrameHeader,
        au: Vec<u8>,
    },
    StreamStop {
        stream_id: u32,
    },
    SpeedTestStart(SpeedTestStart),
    /// Owned form, as for `Frame`; the receiver never builds this variant.
    SpeedTestData {
        test_id: u32,
        filler: Vec<u8>,
    },
    SpeedTestEnd(SpeedTestEnd),
    /// A type this implementation does not know (§3); its payload was skipped.
    Unknown {
        msg_type: u8,
        len: u32,
    },
}

impl Message {
    pub fn msg_type(&self) -> u8 {
        use msg_type as t;
        match self {
            Message::Hello(_) => t::HELLO,
            Message::KeyframeRequest => t::KEYFRAME_REQUEST,
            Message::Heartbeat { .. } => t::HEARTBEAT,
            Message::Stats(_) => t::STATS,
            Message::Goodbye { .. } => t::GOODBYE,
            Message::Ping(_) => t::PING,
            Message::Pong(_) => t::PONG,
            Message::SpeedTestResult(_) => t::SPEED_TEST_RESULT,
            Message::StreamStart(_) => t::STREAM_START,
            Message::Frame { .. } => t::FRAME,
            Message::StreamStop { .. } => t::STREAM_STOP,
            Message::SpeedTestStart(_) => t::SPEED_TEST_START,
            Message::SpeedTestData { .. } => t::SPEED_TEST_DATA,
            Message::SpeedTestEnd(_) => t::SPEED_TEST_END,
            Message::Unknown { msg_type, .. } => *msg_type,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Message::Hello(_) => "HELLO",
            Message::KeyframeRequest => "KEYFRAME_REQUEST",
            Message::Heartbeat { .. } => "HEARTBEAT",
            Message::Stats(_) => "STATS",
            Message::Goodbye { .. } => "GOODBYE",
            Message::Ping(_) => "PING",
            Message::Pong(_) => "PONG",
            Message::SpeedTestResult(_) => "SPEED_TEST_RESULT",
            Message::StreamStart(_) => "STREAM_START",
            Message::Frame { .. } => "FRAME",
            Message::StreamStop { .. } => "STREAM_STOP",
            Message::SpeedTestStart(_) => "SPEED_TEST_START",
            Message::SpeedTestData { .. } => "SPEED_TEST_DATA",
            Message::SpeedTestEnd(_) => "SPEED_TEST_END",
            Message::Unknown { .. } => "UNKNOWN",
        }
    }

    pub fn goodbye(reason: Reason, message: impl Into<String>) -> Message {
        Message::Goodbye {
            reason,
            message: message.into(),
        }
    }

    /// Appends header + payload to `out`.
    ///
    /// `Unknown` keeps only the length of the skipped payload, so it encodes
    /// as that many zero bytes; it exists for skipping, not for forwarding.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0u8; HEADER_LEN]);
        let mut w = Writer(out);
        match self {
            Message::Hello(h) => {
                w.u16(h.protocol_version);
                w.u16(h.caps);
                w.u32(h.panel_width_px);
                w.u32(h.panel_height_px);
                w.u32(h.refresh_mhz);
                w.u16(h.physical_width_mm);
                w.u16(h.physical_height_mm);
                w.u64(h.display_id);
                w.u32(h.codecs);
                w.u32(h.max_bitrate_kbps);
                w.string(&h.name);
            }
            Message::KeyframeRequest => {}
            Message::Heartbeat { mono_us } => w.u64(*mono_us),
            Message::Stats(s) => {
                w.u32(s.stream_id);
                w.u64(s.frames_received);
                w.u64(s.frames_decoded);
                w.u64(s.frames_dropped);
                w.u32(s.avg_decode_us);
                w.u32(s.queue_depth);
            }
            Message::Goodbye { reason, message } => {
                w.u16(reason.0);
                w.string(message);
            }
            Message::Ping(p) | Message::Pong(p) => {
                w.u32(p.seq);
                w.u64(p.t_us);
            }
            Message::SpeedTestResult(r) => {
                w.u32(r.test_id);
                w.u32(r.status);
                w.u64(r.bytes_received);
                w.u64(r.elapsed_us);
            }
            Message::StreamStart(s) => {
                w.u16(s.protocol_version);
                w.u8(s.codec);
                w.u8(s.sflags);
                w.u32(s.width_px);
                w.u32(s.height_px);
                w.u32(s.fps_mhz);
                w.u32(s.stream_id);
            }
            Message::Frame { header, au } => {
                w.bytes(&header.encode());
                w.bytes(au);
            }
            Message::StreamStop { stream_id } => w.u32(*stream_id),
            Message::SpeedTestStart(s) => {
                w.u32(s.test_id);
                w.u32(s.duration_ms);
                w.u32(0);
            }
            Message::SpeedTestData { test_id, filler } => {
                w.bytes(&SpeedTestDataHeader { test_id: *test_id }.encode());
                w.bytes(filler);
            }
            Message::SpeedTestEnd(e) => {
                w.u32(e.test_id);
                w.u64(e.bytes_sent);
            }
            Message::Unknown { len, .. } => w.bytes(&vec![0u8; *len as usize]),
        }
        let payload_len = out.len() - start - HEADER_LEN;
        let header = Header::new(self.msg_type(), payload_len as u32);
        out[start..start + HEADER_LEN].copy_from_slice(&header.encode());
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// Decodes one payload. Unknown types yield `Message::Unknown`.
    pub fn decode(msg_type: u8, payload: &[u8]) -> Result<Message, ProtocolError> {
        use crate::protocol::msg_type as t;
        let fixed = |expected: usize| {
            if payload.len() == expected {
                Ok(Reader::new(payload))
            } else {
                Err(ProtocolError::WrongLength {
                    msg_type,
                    expected,
                    actual: payload.len(),
                })
            }
        };
        let at_least = |min: usize| {
            if payload.len() >= min {
                Ok(Reader::new(payload))
            } else {
                Err(ProtocolError::Truncated { msg_type, min })
            }
        };

        let msg = match msg_type {
            t::HELLO => {
                let mut r = at_least(38)?;
                let hello = Hello {
                    protocol_version: r.u16(),
                    caps: r.u16(),
                    panel_width_px: r.u32(),
                    panel_height_px: r.u32(),
                    refresh_mhz: r.u32(),
                    physical_width_mm: r.u16(),
                    physical_height_mm: r.u16(),
                    display_id: r.u64(),
                    codecs: r.u32(),
                    max_bitrate_kbps: r.u32(),
                    name: r.string()?,
                };
                r.finish(msg_type)?;
                Message::Hello(hello)
            }
            t::KEYFRAME_REQUEST => {
                fixed(0)?;
                Message::KeyframeRequest
            }
            t::HEARTBEAT => Message::Heartbeat {
                mono_us: fixed(8)?.u64(),
            },
            t::STATS => {
                let mut r = fixed(36)?;
                Message::Stats(Stats {
                    stream_id: r.u32(),
                    frames_received: r.u64(),
                    frames_decoded: r.u64(),
                    frames_dropped: r.u64(),
                    avg_decode_us: r.u32(),
                    queue_depth: r.u32(),
                })
            }
            t::GOODBYE => {
                let mut r = at_least(4)?;
                let reason = Reason(r.u16());
                let message = r.string()?;
                r.finish(msg_type)?;
                Message::Goodbye { reason, message }
            }
            t::PING | t::PONG => {
                let mut r = fixed(12)?;
                let p = PingPong {
                    seq: r.u32(),
                    t_us: r.u64(),
                };
                if msg_type == t::PING {
                    Message::Ping(p)
                } else {
                    Message::Pong(p)
                }
            }
            t::SPEED_TEST_RESULT => {
                let mut r = fixed(24)?;
                Message::SpeedTestResult(SpeedTestResult {
                    test_id: r.u32(),
                    status: r.u32(),
                    bytes_received: r.u64(),
                    elapsed_us: r.u64(),
                })
            }
            t::STREAM_START => {
                let mut r = fixed(20)?;
                Message::StreamStart(StreamStart {
                    protocol_version: r.u16(),
                    codec: r.u8(),
                    sflags: r.u8(),
                    width_px: r.u32(),
                    height_px: r.u32(),
                    fps_mhz: r.u32(),
                    stream_id: r.u32(),
                })
            }
            t::FRAME => {
                at_least(FrameHeader::LEN)?;
                let (head, au) = payload.split_at(FrameHeader::LEN);
                Message::Frame {
                    header: FrameHeader::parse(head.try_into().expect("split at LEN")),
                    au: au.to_vec(),
                }
            }
            t::STREAM_STOP => Message::StreamStop {
                stream_id: fixed(4)?.u32(),
            },
            t::SPEED_TEST_START => {
                let mut r = fixed(12)?;
                Message::SpeedTestStart(SpeedTestStart {
                    test_id: r.u32(),
                    duration_ms: r.u32(),
                })
            }
            t::SPEED_TEST_DATA => {
                at_least(SpeedTestDataHeader::LEN)?;
                let (head, filler) = payload.split_at(SpeedTestDataHeader::LEN);
                Message::SpeedTestData {
                    test_id: SpeedTestDataHeader::parse(head.try_into().expect("split at LEN"))
                        .test_id,
                    filler: filler.to_vec(),
                }
            }
            t::SPEED_TEST_END => {
                let mut r = fixed(12)?;
                Message::SpeedTestEnd(SpeedTestEnd {
                    test_id: r.u32(),
                    bytes_sent: r.u64(),
                })
            }
            _ => Message::Unknown {
                msg_type,
                len: payload.len() as u32,
            },
        };
        Ok(msg)
    }

    /// Decodes a complete message (header + payload), e.g. a fixture file.
    pub fn decode_bytes(bytes: &[u8]) -> Result<Message, ProtocolError> {
        let header_bytes: &[u8; HEADER_LEN] = bytes
            .get(..HEADER_LEN)
            .and_then(|h| h.try_into().ok())
            .ok_or(ProtocolError::Invalid("shorter than a message header"))?;
        let header = Header::parse(header_bytes)?;
        let payload = &bytes[HEADER_LEN..];
        if payload.len() != header.length as usize {
            return Err(ProtocolError::Invalid("header length does not match bytes"));
        }
        Message::decode(header.msg_type, payload)
    }
}

/// Big-endian cursor over a payload whose length the caller already checked.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take<const N: usize>(&mut self) -> [u8; N] {
        let out = self.buf[self.pos..self.pos + N]
            .try_into()
            .expect("length checked by caller");
        self.pos += N;
        out
    }

    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take())
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take())
    }

    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take())
    }

    /// A `u16`-length-prefixed UTF-8 string; the length prefix itself must
    /// already be covered by the caller's minimum-length check.
    fn string(&mut self) -> Result<String, ProtocolError> {
        let len = self.u16() as usize;
        let bytes = self
            .buf
            .get(self.pos..self.pos + len)
            .ok_or(ProtocolError::StringOverrun)?;
        self.pos += len;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::BadUtf8)
    }

    fn finish(&self, msg_type: u8) -> Result<(), ProtocolError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ProtocolError::WrongLength {
                msg_type,
                expected: self.pos,
                actual: self.buf.len(),
            })
        }
    }
}

struct Writer<'a>(&'a mut Vec<u8>);

impl Writer<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn bytes(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }

    /// Strings longer than `u16::MAX` bytes are truncated at a char boundary.
    fn string(&mut self, s: &str) {
        let mut end = s.len().min(u16::MAX as usize);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.u16(end as u16);
        self.bytes(&s.as_bytes()[..end]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("protocol/fixtures")
    }

    /// Every fixture in PROTOCOL.md §12 with the message it must contain.
    fn fixture_messages() -> Vec<(&'static str, Message)> {
        vec![
            (
                "hello_imac.bin",
                Message::Hello(Hello {
                    protocol_version: 1,
                    caps: 0,
                    panel_width_px: 5120,
                    panel_height_px: 2880,
                    refresh_mhz: 60_000,
                    physical_width_mm: 597,
                    physical_height_mm: 336,
                    display_id: u64::from_be_bytes(*b"IMAC2017"),
                    codecs: CODECS_HEVC_MAIN,
                    max_bitrate_kbps: 0,
                    name: "iMac".into(),
                }),
            ),
            (
                "stream_start_5k.bin",
                Message::StreamStart(StreamStart {
                    protocol_version: 1,
                    codec: CODEC_HEVC_MAIN,
                    sflags: SFLAG_HIDPI,
                    width_px: 5120,
                    height_px: 2880,
                    fps_mhz: 60_000,
                    stream_id: 1,
                }),
            ),
            (
                "frame_keyframe_min.bin",
                Message::Frame {
                    header: FrameHeader {
                        pts_us: 1_000_000,
                        fflags: FFLAG_KEYFRAME,
                    },
                    au: vec![0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01],
                },
            ),
            (
                "frame_delta_min.bin",
                Message::Frame {
                    header: FrameHeader {
                        pts_us: 1_016_667,
                        fflags: 0,
                    },
                    au: vec![0, 0, 0, 1],
                },
            ),
            ("keyframe_request.bin", Message::KeyframeRequest),
            (
                "heartbeat.bin",
                Message::Heartbeat {
                    mono_us: 123_456_789,
                },
            ),
            ("goodbye_busy.bin", Message::goodbye(Reason::BUSY, "busy")),
            ("stream_stop_1.bin", Message::StreamStop { stream_id: 1 }),
            (
                "stats.bin",
                Message::Stats(Stats {
                    stream_id: 1,
                    frames_received: 600,
                    frames_decoded: 598,
                    frames_dropped: 2,
                    avg_decode_us: 3500,
                    queue_depth: 1,
                }),
            ),
            (
                "ping.bin",
                Message::Ping(PingPong {
                    seq: 7,
                    t_us: 5_000_000,
                }),
            ),
            (
                "pong.bin",
                Message::Pong(PingPong {
                    seq: 7,
                    t_us: 5_000_000,
                }),
            ),
            (
                "speed_test_start.bin",
                Message::SpeedTestStart(SpeedTestStart {
                    test_id: 1,
                    duration_ms: 1000,
                }),
            ),
            (
                "speed_test_data_min.bin",
                Message::SpeedTestData {
                    test_id: 1,
                    filler: vec![0xA5; 16],
                },
            ),
            (
                "speed_test_end.bin",
                Message::SpeedTestEnd(SpeedTestEnd {
                    test_id: 1,
                    bytes_sent: 125_000_000,
                }),
            ),
            (
                "speed_test_result.bin",
                Message::SpeedTestResult(SpeedTestResult {
                    test_id: 1,
                    status: SPEED_TEST_OK,
                    bytes_received: 125_000_000,
                    elapsed_us: 1_000_000,
                }),
            ),
        ]
    }

    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits
            .chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn every_fixture_round_trips_byte_for_byte() {
        let mut count = 0;
        for entry in std::fs::read_dir(fixtures_dir()).expect("fixtures dir") {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "bin") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let msg =
                Message::decode_bytes(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                !matches!(msg, Message::Unknown { .. }),
                "{} decoded as Unknown",
                path.display()
            );
            assert_eq!(
                msg.to_bytes(),
                bytes,
                "{} re-encodes differently",
                path.display()
            );
            count += 1;
        }
        assert_eq!(count, fixture_messages().len(), "fixture count");
    }

    #[test]
    fn fixtures_contain_the_values_in_protocol_section_12() {
        for (name, expected) in fixture_messages() {
            let bytes = std::fs::read(fixtures_dir().join(name)).unwrap();
            assert_eq!(Message::decode_bytes(&bytes).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn hello_fixture_matches_protocol_hex() {
        let spec = hex("01 00 00 00 00 00 00 2A
             00 01 00 00 00 00 14 00 00 00 0B 40 00 00 EA 60 02 55 01 50
             49 4D 41 43 32 30 31 37 00 00 00 01 00 00 00 00
             00 04 69 4D 61 63");
        let fixture = std::fs::read(fixtures_dir().join("hello_imac.bin")).unwrap();
        assert_eq!(fixture, spec);
    }

    /// Regenerates every fixture into a temp dir and checks it against the
    /// committed copy. Never writes into `protocol/fixtures/`: those bytes are
    /// the cross-language contract and change only with a protocol bump.
    #[test]
    #[ignore = "writes files; run explicitly with --ignored"]
    fn gen_fixtures() {
        let out = std::env::temp_dir().join(format!("idp-fixtures-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        for (name, msg) in fixture_messages() {
            let path = out.join(name);
            std::fs::write(&path, msg.to_bytes()).unwrap();
            let committed = std::fs::read(fixtures_dir().join(name)).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), committed, "{name}");
        }
        eprintln!("fixtures written to {}", out.display());
    }

    #[test]
    fn header_rejects_oversized_length() {
        let bytes = Header::new(msg_type::FRAME, MAX_PAYLOAD_LEN + 1).encode();
        assert_eq!(
            Header::parse(&bytes),
            Err(ProtocolError::TooLong(MAX_PAYLOAD_LEN + 1))
        );
        let ok = Header::new(msg_type::FRAME, MAX_PAYLOAD_LEN).encode();
        assert!(Header::parse(&ok).is_ok());
    }

    #[test]
    fn unknown_types_are_reported_with_their_length() {
        let msg = Message::decode(0xFF, &[1, 2, 3]).unwrap();
        assert_eq!(
            msg,
            Message::Unknown {
                msg_type: 0xFF,
                len: 3
            }
        );
    }

    #[test]
    fn fixed_payloads_with_wrong_length_are_errors() {
        assert!(matches!(
            Message::decode(msg_type::STATS, &[0u8; 35]),
            Err(ProtocolError::WrongLength { expected: 36, .. })
        ));
        assert!(matches!(
            Message::decode(msg_type::PING, &[0u8; 13]),
            Err(ProtocolError::WrongLength { .. })
        ));
        assert!(Message::decode(msg_type::KEYFRAME_REQUEST, &[0]).is_err());
    }

    #[test]
    fn strings_past_the_payload_are_errors() {
        // GOODBYE claiming a 10-byte message but carrying 2.
        let payload = [0, 0, 0, 10, b'h', b'i'];
        assert_eq!(
            Message::decode(msg_type::GOODBYE, &payload),
            Err(ProtocolError::StringOverrun)
        );
        assert!(matches!(
            Message::decode(msg_type::GOODBYE, &[0, 0, 0, 0, 0xFF]),
            Err(ProtocolError::WrongLength { .. })
        ));
    }

    #[test]
    fn speed_test_start_validation() {
        let ok = SpeedTestStart {
            test_id: 1,
            duration_ms: 100,
        };
        assert!(ok.validate().is_ok());
        assert!(SpeedTestStart { test_id: 0, ..ok }.validate().is_err());
        assert!(
            SpeedTestStart {
                duration_ms: 99,
                ..ok
            }
            .validate()
            .is_err()
        );
        assert!(
            SpeedTestStart {
                duration_ms: 10_001,
                ..ok
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn speed_test_result_throughput() {
        let r = SpeedTestResult {
            test_id: 1,
            status: 0,
            bytes_received: 125_000_000,
            elapsed_us: 1_000_000,
        };
        assert_eq!(r.kbit_per_s(), Some(1_000_000));
    }

    #[test]
    fn reserved_bits_are_detected() {
        let frame = |b: &[u8]| {
            let mut p = [0u8; FrameHeader::LEN];
            p[8..8 + b.len()].copy_from_slice(b);
            payload_has_reserved_bits(msg_type::FRAME, &p)
        };
        assert!(!frame(&[FFLAG_KEYFRAME]));
        assert!(frame(&[0x02]));
        assert!(frame(&[0, 0, 0, 1]));

        let mut start = [0u8; 20];
        start[3] = SFLAG_HIDPI;
        assert!(!payload_has_reserved_bits(msg_type::STREAM_START, &start));
        start[3] |= 0x80;
        assert!(payload_has_reserved_bits(msg_type::STREAM_START, &start));

        let mut speed = [0u8; 12];
        assert!(!payload_has_reserved_bits(
            msg_type::SPEED_TEST_START,
            &speed
        ));
        speed[11] = 1;
        assert!(payload_has_reserved_bits(
            msg_type::SPEED_TEST_START,
            &speed
        ));
        assert!(payload_has_reserved_bits(
            msg_type::SPEED_TEST_DATA,
            &speed[4..]
        ));
        assert!(!payload_has_reserved_bits(
            msg_type::SPEED_TEST_DATA,
            &speed[..8]
        ));

        for fixture in [
            "frame_keyframe_min.bin",
            "stream_start_5k.bin",
            "speed_test_start.bin",
        ] {
            let bytes = std::fs::read(fixtures_dir().join(fixture)).unwrap();
            assert!(
                !payload_has_reserved_bits(bytes[0], &bytes[HEADER_LEN..]),
                "{fixture}"
            );
        }
    }

    #[test]
    fn frame_header_round_trip() {
        let h = FrameHeader {
            pts_us: 42,
            fflags: FFLAG_KEYFRAME,
        };
        assert_eq!(FrameHeader::parse(&h.encode()), h);
        assert!(h.is_keyframe());
        assert_eq!(FrameHeader::message_header(4).unwrap().length, 16);
    }
}
