//! Framed message I/O over TCP (PROTOCOL.md §4) and the listening socket.
//!
//! The reader streams the two bulk message types instead of buffering them:
//! FRAME access units go straight into a `gst::Buffer`, SPEED_TEST_DATA
//! filler is read through a fixed scratch buffer and dropped (§6.9).

use std::collections::HashSet;
use std::io::{self, BufReader, Read, Write};
use std::net::{Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use thiserror::Error;

use crate::protocol::{
    FrameHeader, HEADER_LEN, Header, Message, ProtocolError, SpeedTestDataHeader, msg_type,
    payload_has_reserved_bits,
};

const READ_BUFFER: usize = 256 * 1024;
const SCRATCH_LEN: usize = 64 * 1024;
/// Longest valid control payload: HELLO's fixed part plus a maximal string.
const MAX_CONTROL_LEN: u32 = 38 + u16::MAX as u32;
/// Bounds how long a heartbeat or PONG may block on a peer that stopped reading.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum NetError {
    #[error("connection closed by peer")]
    Closed,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("buffer allocation failed: {0}")]
    Alloc(String),
}

/// One message as delivered by `FramedReader`.
#[derive(Debug)]
pub enum Inbound {
    /// Any message other than FRAME and SPEED_TEST_DATA, including
    /// `Message::Unknown` for skipped types.
    Message(Message),
    Frame {
        header: FrameHeader,
        au: gst::Buffer,
    },
    SpeedTestData {
        test_id: u32,
        filler_len: u64,
    },
}

pub struct FramedReader<R> {
    inner: BufReader<R>,
    payload: Vec<u8>,
    scratch: Box<[u8]>,
    /// (message type, part) already logged for non-zero reserved bits; once
    /// each keeps a misbehaving sender from logging on every FRAME.
    reserved_logged: HashSet<(u8, &'static str)>,
}

impl<R: Read> FramedReader<R> {
    /// `gst::init()` must have been called (FRAME payloads are `gst::Buffer`s).
    pub fn new(inner: R) -> Self {
        FramedReader {
            inner: BufReader::with_capacity(READ_BUFFER, inner),
            payload: Vec::new(),
            scratch: vec![0u8; SCRATCH_LEN].into_boxed_slice(),
            reserved_logged: HashSet::new(),
        }
    }

    pub fn read(&mut self) -> Result<Inbound, NetError> {
        let mut header_bytes = [0u8; HEADER_LEN];
        self.read_exact(&mut header_bytes)?;
        let header = Header::parse(&header_bytes)?;
        if header.has_reserved_bits() {
            self.log_reserved(header.msg_type, "header");
        }
        let len = header.length;
        match header.msg_type {
            msg_type::FRAME => {
                let mut prefix = [0u8; FrameHeader::LEN];
                self.read_prefix(header.msg_type, len, &mut prefix)?;
                self.check_reserved(header.msg_type, &prefix);
                let au = self.read_into_gst_buffer(len as usize - FrameHeader::LEN)?;
                Ok(Inbound::Frame {
                    header: FrameHeader::parse(&prefix),
                    au,
                })
            }
            msg_type::SPEED_TEST_DATA => {
                let mut prefix = [0u8; SpeedTestDataHeader::LEN];
                self.read_prefix(header.msg_type, len, &mut prefix)?;
                self.check_reserved(header.msg_type, &prefix);
                let filler_len = u64::from(len) - SpeedTestDataHeader::LEN as u64;
                self.discard(filler_len)?;
                Ok(Inbound::SpeedTestData {
                    test_id: SpeedTestDataHeader::parse(&prefix).test_id,
                    filler_len,
                })
            }
            t if is_control(t) => {
                if len > MAX_CONTROL_LEN {
                    return Err(ProtocolError::Invalid("control message too long").into());
                }
                let mut payload = std::mem::take(&mut self.payload);
                payload.resize(len as usize, 0);
                let read = self.read_exact(&mut payload);
                let decoded = read.and_then(|_| Ok(Message::decode(t, &payload)?));
                if decoded.is_ok() {
                    self.check_reserved(t, &payload);
                }
                self.payload = payload;
                Ok(Inbound::Message(decoded?))
            }
            t => {
                log::debug!("[net] skipping unknown message type {t:#04x} ({len} bytes)");
                self.discard(u64::from(len))?;
                Ok(Inbound::Message(Message::Unknown { msg_type: t, len }))
            }
        }
    }

    /// §9: tolerated, but SHOULD be logged.
    fn check_reserved(&mut self, msg_type: u8, payload: &[u8]) {
        if payload_has_reserved_bits(msg_type, payload) {
            self.log_reserved(msg_type, "payload");
        }
    }

    fn log_reserved(&mut self, msg_type: u8, part: &'static str) {
        if self.reserved_logged.insert((msg_type, part)) {
            log::warn!(
                "[net] type {msg_type:#04x}: non-zero reserved {part} bits ignored \
                 (logged once per connection)"
            );
        }
    }

    fn read_prefix(&mut self, msg_type: u8, len: u32, prefix: &mut [u8]) -> Result<(), NetError> {
        if (len as usize) < prefix.len() {
            return Err(ProtocolError::Truncated {
                msg_type,
                min: prefix.len(),
            }
            .into());
        }
        self.read_exact(prefix)
    }

    fn read_into_gst_buffer(&mut self, len: usize) -> Result<gst::Buffer, NetError> {
        let mut buffer = gst::Buffer::with_size(len).map_err(|e| NetError::Alloc(e.to_string()))?;
        {
            let buffer = buffer
                .get_mut()
                .expect("freshly allocated buffer is unique");
            let mut map = buffer
                .map_writable()
                .map_err(|e| NetError::Alloc(e.to_string()))?;
            self.read_exact(&mut map)?;
        }
        Ok(buffer)
    }

    fn discard(&mut self, mut remaining: u64) -> Result<(), NetError> {
        while remaining > 0 {
            let n = remaining.min(self.scratch.len() as u64) as usize;
            let mut scratch = std::mem::take(&mut self.scratch);
            let read = self.read_exact(&mut scratch[..n]);
            self.scratch = scratch;
            read?;
            remaining -= n as u64;
        }
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), NetError> {
        self.inner.read_exact(buf).map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => NetError::Closed,
            _ => NetError::Io(e),
        })
    }
}

fn is_control(t: u8) -> bool {
    use msg_type::*;
    matches!(
        t,
        HELLO
            | KEYFRAME_REQUEST
            | HEARTBEAT
            | STATS
            | GOODBYE
            | PING
            | PONG
            | SPEED_TEST_RESULT
            | STREAM_START
            | STREAM_STOP
            | SPEED_TEST_START
            | SPEED_TEST_END
    )
}

pub struct FramedWriter<W> {
    inner: W,
    buf: Vec<u8>,
    last_tx: Instant,
}

impl<W: Write> FramedWriter<W> {
    pub fn new(inner: W) -> Self {
        FramedWriter {
            inner,
            buf: Vec::with_capacity(256),
            last_tx: Instant::now(),
        }
    }

    pub fn send(&mut self, msg: &Message) -> io::Result<()> {
        self.buf.clear();
        msg.encode(&mut self.buf);
        self.inner.write_all(&self.buf)?;
        self.last_tx = Instant::now();
        Ok(())
    }

    /// Writes a FRAME as one header write and one payload write (§4).
    pub fn send_frame(&mut self, header: FrameHeader, au: &[u8]) -> io::Result<()> {
        let msg_header = FrameHeader::message_header(au.len()).map_err(io::Error::other)?;
        let mut head = [0u8; HEADER_LEN + FrameHeader::LEN];
        head[..HEADER_LEN].copy_from_slice(&msg_header.encode());
        head[HEADER_LEN..].copy_from_slice(&header.encode());
        self.inner.write_all(&head)?;
        self.inner.write_all(au)?;
        self.last_tx = Instant::now();
        Ok(())
    }

    /// Writes one SPEED_TEST_DATA message carrying `filler`.
    pub fn send_speed_test_data(&mut self, test_id: u32, filler: &[u8]) -> io::Result<()> {
        let len =
            u32::try_from(SpeedTestDataHeader::LEN + filler.len()).map_err(io::Error::other)?;
        let mut head = [0u8; HEADER_LEN + SpeedTestDataHeader::LEN];
        head[..HEADER_LEN].copy_from_slice(&Header::new(msg_type::SPEED_TEST_DATA, len).encode());
        head[HEADER_LEN..].copy_from_slice(&SpeedTestDataHeader { test_id }.encode());
        self.inner.write_all(&head)?;
        self.inner.write_all(filler)?;
        self.last_tx = Instant::now();
        Ok(())
    }

    pub fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_tx)
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }
}

/// Binds the receiver port on all interfaces: dual-stack `[::]` where the
/// host supports it, otherwise IPv4 `0.0.0.0`.
pub fn bind(port: u16) -> io::Result<TcpListener> {
    let dual_stack = || -> io::Result<TcpListener> {
        let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_only_v6(false)?;
        socket.set_reuse_address(true)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
        socket.listen(16)?;
        Ok(socket.into())
    };
    dual_stack().or_else(|e| {
        log::info!("[net] IPv6 dual-stack bind unavailable ({e}); using IPv4 only");
        TcpListener::bind(("0.0.0.0", port))
    })
}

/// Per-connection socket options (§4: TCP_NODELAY; bounded blocking writes).
pub fn configure(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))
}

/// Microseconds on this process's monotonic clock, for HEARTBEAT and PING.
pub fn mono_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FFLAG_KEYFRAME, PingPong, Reason};
    use std::io::Cursor;

    fn reader(bytes: Vec<u8>) -> FramedReader<Cursor<Vec<u8>>> {
        gst::init().unwrap();
        FramedReader::new(Cursor::new(bytes))
    }

    #[test]
    fn reads_control_frame_and_speed_data_in_sequence() {
        let mut wire = Vec::new();
        let ping = Message::Ping(PingPong { seq: 1, t_us: 2 });
        ping.encode(&mut wire);
        let mut w = FramedWriter::new(Vec::new());
        w.send_frame(
            FrameHeader {
                pts_us: 9,
                fflags: FFLAG_KEYFRAME,
            },
            &[0, 0, 0, 1, 0x40],
        )
        .unwrap();
        w.send_speed_test_data(3, &[0xA5; 200_000]).unwrap();
        wire.extend_from_slice(w.get_ref());
        Message::goodbye(Reason::NORMAL, "").encode(&mut wire);

        let mut r = reader(wire);
        assert!(matches!(r.read().unwrap(), Inbound::Message(m) if m == ping));
        match r.read().unwrap() {
            Inbound::Frame { header, au } => {
                assert!(header.is_keyframe());
                assert_eq!(header.pts_us, 9);
                assert_eq!(au.map_readable().unwrap().as_slice(), &[0, 0, 0, 1, 0x40]);
            }
            other => panic!("expected frame, got {other:?}"),
        }
        assert!(matches!(
            r.read().unwrap(),
            Inbound::SpeedTestData {
                test_id: 3,
                filler_len: 200_000
            }
        ));
        assert!(matches!(
            r.read().unwrap(),
            Inbound::Message(Message::Goodbye { .. })
        ));
        assert!(matches!(r.read(), Err(NetError::Closed)));
    }

    #[test]
    fn skips_unknown_types_without_desync() {
        let mut wire = Header::new(0x42, 5).encode().to_vec();
        wire.extend_from_slice(&[1, 2, 3, 4, 5]);
        Message::KeyframeRequest.encode(&mut wire);
        let mut r = reader(wire);
        assert!(matches!(
            r.read().unwrap(),
            Inbound::Message(Message::Unknown {
                msg_type: 0x42,
                len: 5
            })
        ));
        assert!(matches!(
            r.read().unwrap(),
            Inbound::Message(Message::KeyframeRequest)
        ));
    }

    #[test]
    fn rejects_oversized_and_truncated_messages() {
        let wire = Header::new(msg_type::FRAME, 64 * 1024 * 1024 + 1)
            .encode()
            .to_vec();
        assert!(matches!(
            reader(wire).read(),
            Err(NetError::Protocol(ProtocolError::TooLong(_)))
        ));

        let mut wire = Header::new(msg_type::FRAME, 4).encode().to_vec();
        wire.extend_from_slice(&[0; 4]);
        assert!(matches!(
            reader(wire).read(),
            Err(NetError::Protocol(ProtocolError::Truncated { .. }))
        ));

        let wire = Header::new(msg_type::GOODBYE, MAX_CONTROL_LEN + 1)
            .encode()
            .to_vec();
        assert!(matches!(reader(wire).read(), Err(NetError::Protocol(_))));
    }

    #[test]
    fn eof_mid_message_is_closed() {
        let mut wire = Message::Heartbeat { mono_us: 1 }.to_bytes();
        wire.truncate(10);
        assert!(matches!(reader(wire).read(), Err(NetError::Closed)));
    }
}
