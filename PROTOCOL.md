# iMac Display Protocol (IDP) — version 1

Status: normative. This document is the single source of truth for everything
that crosses the wire between a **sender** (the machine whose desktop is being
extended, e.g. a MacBook) and a **receiver** (the machine whose panel shows the
picture, e.g. an iMac running Ubuntu).

If code and this document disagree, the code is wrong. Any change to the wire
format requires a protocol version bump (§3) and an update to this file in the
same pull request.

Terminology: MUST, MUST NOT, SHOULD, MAY are used as in RFC 2119.

---

## 1. Overview

```
 sender (Mac)                                   receiver (Ubuntu)
 ─────────────                                  ─────────────────
 creates virtual display  ──── TCP connect ────▶ listening on 0.0.0.0:7878
                          ◀──── HELLO ─────────  advertises panel geometry
 STREAM_START ───────────────────────────────▶   configures decoder
 FRAME (keyframe) ───────────────────────────▶   first picture
 FRAME, FRAME, FRAME ... ───────────────────▶
                          ◀──── KEYFRAME_REQUEST (on demand)
 HEARTBEAT ◀────────────────────────────────▶ HEARTBEAT (both ways, ≤500 ms)
 PING ───────────────────────────────────────▶   (either side may ping)
                          ◀──── PONG ──────────  round-trip time
 SPEED_TEST_START, SPEED_TEST_DATA ..., SPEED_TEST_END ─▶ (on demand, §6.9)
                          ◀──── SPEED_TEST_RESULT  measured link throughput
 GOODBYE / close ────────────────────────────▶   blanks panel, listens again
```

- Transport: one TCP connection, opened by the sender. The receiver listens on
  **all interfaces** on one configurable port (default **7878**). The sender is
  given the receiver's IP at launch, so the same binaries work over a
  Thunderbolt bridge, Ethernet, or any other IP link.
- Video: HEVC (H.265) Main profile, 8-bit 4:2:0, Annex-B byte stream, one
  access unit per FRAME message (§6.2).
- Direction: video flows sender → receiver only. Control flows both ways.
- Reliability: TCP provides ordering and delivery. The protocol relies on TCP
  backpressure for flow control (§8) and has no retransmission layer.

---

## 2. Conventions

- All integers are **big-endian**, unsigned unless stated.
- `u8/u16/u32/u64` are fixed-width integers. `string` is `u16 length` followed
  by that many bytes of UTF-8 (no terminator). Length 0 is a valid empty string.
- Reserved fields MUST be sent as zero and MUST be ignored when read.
- Times in `_us` fields are microseconds. Rates in `_mhz` fields are
  millihertz (60.000 Hz = `60000`).
- "Close" means a normal TCP close (FIN). Either side MAY also drop the
  connection abruptly; peers MUST treat any read error or EOF as disconnect.

---

## 3. Versioning

- `PROTOCOL_VERSION = 1`.
- The receiver states its version in HELLO; the sender states its own in
  STREAM_START. Only identical major versions interoperate in v1 (there is a
  single `u16`; treat it as the major version).
- On mismatch, the side that detects it MUST send GOODBYE with reason
  `VERSION_MISMATCH` and close.
- Forward compatibility inside a version: a peer MUST ignore message types it
  does not know (skip `length` bytes). New *optional* message types may be added
  without a version bump; changes to existing layouts require a bump.

---

## 4. Message framing

Every message, in both directions, has an 8-byte header followed by `length`
payload bytes.

```
offset  size  field      notes
0       u8    type       see §5
1       u8    hflags     header flags, reserved, MUST be 0
2       u16   reserved   MUST be 0
4       u32   length     payload length in bytes, 0 ≤ length ≤ 67 108 864 (64 MiB)
8       ...   payload
```

- A `length` above 64 MiB is a protocol error (§9).
- Messages MUST NOT be interleaved; each message is written contiguously.
- Implementations SHOULD set `TCP_NODELAY` and write each FRAME with as few
  `write` calls as practical (one header write and one payload write is fine;
  a single writev is better).

---

## 5. Message types

| type | name             | direction           | payload              | §    |
|------|------------------|---------------------|----------------------|------|
| 0x01 | HELLO            | receiver → sender   | fixed + string       | 6.1  |
| 0x02 | KEYFRAME_REQUEST | receiver → sender   | empty                | 6.4  |
| 0x03 | HEARTBEAT        | both                | 8 bytes              | 6.5  |
| 0x04 | STATS            | receiver → sender   | fixed (optional)     | 6.7  |
| 0x05 | GOODBYE          | both                | u16 + string         | 6.6  |
| 0x06 | PING             | both                | 12 bytes             | 6.8  |
| 0x07 | PONG             | both                | 12 bytes             | 6.8  |
| 0x08 | SPEED_TEST_RESULT| receiver → sender   | 24 bytes             | 6.9  |
| 0x10 | STREAM_START     | sender → receiver   | 20 bytes             | 6.3  |
| 0x11 | FRAME            | sender → receiver   | 12 bytes + AU        | 6.2  |
| 0x12 | STREAM_STOP      | sender → receiver   | 4 bytes              | 6.3  |
| 0x13 | SPEED_TEST_START | sender → receiver   | 12 bytes             | 6.9  |
| 0x14 | SPEED_TEST_DATA  | sender → receiver   | 8 bytes + filler     | 6.9  |
| 0x15 | SPEED_TEST_END   | sender → receiver   | 12 bytes             | 6.9  |

Types 0x09–0x0F and 0x16–0xFF are unassigned. Unknown types MUST be skipped.

---

## 6. Payloads

### 6.1 HELLO (0x01), receiver → sender

Sent by the receiver **immediately** after accepting the connection, before
reading anything. Describes the panel so the sender can create a matching
virtual display.

```
offset  size  field               notes
0       u16   protocol_version    = 1
2       u16   caps                bit 0: receiver can scale non-native sizes
                                  bit 1: receiver supports STATS
                                  bit 2: receiver supports speed test (§6.9)
                                  other bits reserved (0)
4       u32   panel_width_px      native panel width in pixels, e.g. 5120
8       u32   panel_height_px     native panel height in pixels, e.g. 2880
12      u32   refresh_mhz         native refresh, e.g. 60000
16      u16   physical_width_mm   active area width, 0 if unknown
18      u16   physical_height_mm  active area height, 0 if unknown
20      u64   display_id          stable identity of this receiver (§6.1.1)
28      u32   codecs              bit 0: HEVC Main 4:2:0 8-bit. MUST be set in v1.
32      u32   max_bitrate_kbps    hint, 0 = no preference
36      string name               human-readable, e.g. "iMac"
```

#### 6.1.1 display_id

The sender uses `display_id` to derive the virtual display's vendor/product/
serial so that macOS remembers arrangement and resolution between sessions.
It MUST be stable across reboots of the receiver and SHOULD be unique per
receiver. Recommended derivation: first 8 bytes of SHA-256 over
`/etc/machine-id` + the DRM connector name (e.g. `eDP-1`). A sender MUST treat
it as opaque.

#### 6.1.2 Example HELLO (hex)

Receiver: 5120×2880 @ 60 Hz, 597×336 mm, id `"IMAC2017"` as ASCII bytes,
HEVC only, no bitrate hint, name `"iMac"`, caps = 0.

```
01 00 00 00 00 00 00 2A                          header: type=HELLO, length=42
00 01                                            protocol_version = 1
00 00                                            caps = 0
00 00 14 00                                      panel_width_px = 5120
00 00 0B 40                                      panel_height_px = 2880
00 00 EA 60                                      refresh_mhz = 60000
02 55                                            physical_width_mm = 597
01 50                                            physical_height_mm = 336
49 4D 41 43 32 30 31 37                          display_id = "IMAC2017"
00 00 00 01                                      codecs = HEVC
00 00 00 00                                      max_bitrate_kbps = 0
00 04 69 4D 61 63                                name = "iMac"
```

Both implementations MUST include this exact byte sequence as a unit-test
fixture (`protocol/fixtures/hello_imac.bin`).

### 6.2 FRAME (0x11), sender → receiver

One encoded video access unit.

```
offset  size  field       notes
0       u64   pts_us      presentation time on the sender's monotonic clock
8       u8    fflags      bit 0: keyframe (IDR/CRA with parameter sets in-band)
                          other bits reserved (0)
9       u8[3] reserved    MUST be 0
12      ...   access unit Annex-B HEVC, see constraints below
```

Access unit constraints (all MUST):

- HEVC Main profile, 8-bit, 4:2:0 (`chroma_format_idc = 1`, `bit_depth = 8`).
  Receivers MAY reject anything else with GOODBYE `UNSUPPORTED_CODEC`.
- Annex-B byte stream: every NAL unit prefixed with `00 00 00 01`
  (4-byte start code; receivers MUST also accept 3-byte `00 00 01`).
- Exactly one picture per FRAME (AU alignment).
- A FRAME with `keyframe` set MUST contain VPS, SPS and PPS NAL units before the
  slice NALs, and the picture MUST be decodable with no reference to earlier
  frames (IDR). A receiver MAY begin decoding a stream only at such a frame.
- No B-frames / no frame reordering: decode order == presentation order, and
  `pts_us` MUST be strictly increasing within a stream.
- Payload size ≤ 64 MiB (framing limit). Realistically keyframes are < 4 MiB.

`pts_us` has an arbitrary epoch; receivers MUST rebase on the first FRAME of
each stream (§6.3) and MUST NOT compare it to their own clock.

### 6.3 STREAM_START (0x10) and STREAM_STOP (0x12), sender → receiver

STREAM_START MUST precede the first FRAME and MUST be followed by a keyframe
FRAME before any non-keyframe FRAME. It is also how the sender changes
resolution: send STREAM_STOP, then a new STREAM_START with a new `stream_id`,
then a keyframe.

```
STREAM_START
offset  size  field              notes
0       u16   protocol_version   = 1
2       u8    codec              1 = HEVC Main 4:2:0 8-bit (only value in v1)
3       u8    sflags             bit 0: hidpi (content is 2× scaled; informational)
4       u32   width_px           coded width, ≤ panel_width_px
8       u32   height_px          coded height, ≤ panel_height_px
12      u32   fps_mhz            nominal frame rate, e.g. 60000; 0 = variable
16      u32   stream_id          sender-chosen, MUST differ from the previous one
                                 on this connection (start at 1, increment)

STREAM_STOP
0       u32   stream_id          the stream being stopped
```

Receiver behaviour:

- On STREAM_START: tear down any existing decoder, configure for the new
  geometry, discard FRAMEs until the first keyframe.
- `width_px`/`height_px` SHOULD equal the native panel size. If smaller, a
  receiver with `caps` bit 0 set scales to the panel; otherwise it centres the
  image on black. Larger than the panel is a protocol error.
- On STREAM_STOP: stop decoding, show black (or a "paused" indicator), keep
  the connection open. FRAMEs received after STREAM_STOP and before a new
  STREAM_START MUST be ignored.

### 6.4 KEYFRAME_REQUEST (0x02), receiver → sender

Empty payload. The sender MUST respond by making the next FRAME it sends a
keyframe (§6.2). A receiver MUST NOT send this more often than once per
second. A receiver SHOULD send it when:

- it has received STREAM_START but no keyframe within 500 ms;
- its decoder reports an error;
- it has just resumed from a local fault (e.g. pipeline restart).

The sender SHOULD also send a keyframe unprompted at STREAM_START. Senders
SHOULD otherwise set their encoder's maximum keyframe interval very high
(effectively infinite), because TCP never loses data and periodic keyframes are
pure bandwidth cost.

### 6.5 HEARTBEAT (0x03), both directions

```
0       u64   mono_us   sender-of-this-message's monotonic clock, informational
```

Liveness rule: each side MUST send **some** message at least every **500 ms**.
If it has nothing else to send (sender with an idle screen, receiver at any
time), it sends HEARTBEAT. Any received message counts as liveness, including
FRAME.

If a side receives nothing for **2000 ms**, it MUST treat the peer as dead:
close the socket and take its disconnect actions (§7). Do not send GOODBYE in
this case; the peer is presumed gone.

### 6.6 GOODBYE (0x05), both directions

```
0       u16     reason     see table
2       string  message    free text for logs, MAY be empty
```

| reason | name              | meaning                                           |
|--------|-------------------|---------------------------------------------------|
| 0      | NORMAL            | orderly shutdown (user quit, unplug)              |
| 1      | VERSION_MISMATCH  | protocol_version differs                          |
| 2      | BUSY              | receiver already has an active sender             |
| 3      | UNSUPPORTED_CODEC | codec/profile not decodable                       |
| 4      | TIMEOUT           | handshake step did not complete in time           |
| 5      | PROTOCOL_ERROR    | malformed message, bad length, wrong state        |
| 6      | INTERNAL_ERROR    | encoder/decoder/display failure                   |

After sending GOODBYE a side MUST close the connection. After receiving GOODBYE
a side MUST NOT send anything further and SHOULD close.

### 6.7 STATS (0x04), receiver → sender, optional

Sent at most every 1000 ms when `caps` bit 1 is set. Senders MAY use it for
adaptive bitrate; v1 senders MAY ignore it.

```
0       u32   stream_id
4       u64   frames_received
12      u64   frames_decoded
20      u64   frames_dropped       decoded but not shown (late)
28      u32   avg_decode_us        mean decode time over the last interval
32      u32   queue_depth          encoded frames waiting for the decoder
```

Payload is exactly 36 bytes. Counters are cumulative for `stream_id` and reset
on each STREAM_START.

### 6.8 PING (0x06) and PONG (0x07), both directions

Round-trip time measurement. Either side MAY send PING at any time after HELLO
(the receiver after sending HELLO, the sender after receiving it). Every v1
implementation MUST answer PING.

```
PING and PONG (identical layout)
offset  size  field     notes
0       u32   seq       chosen by the PING's sender, echoed in PONG
4       u64   t_us      PING sender's monotonic clock; echoed unchanged in PONG
```

- On PING, the peer MUST reply with PONG carrying the same `seq` and `t_us`,
  as soon as practical (before any queued FRAME it has not started writing,
  where the implementation allows it). The PING sender computes
  `rtt_us = now_us - t_us` on its own clock.
- A side SHOULD NOT send more than 10 PINGs per second. An unsolicited or
  unmatched PONG is ignored.
- PING/PONG count as liveness like any other message (§6.5).
- RTT measured this way includes the peer's queueing delay. It is a live link
  health indicator, not a precise network RTT; senders SHOULD display it as
  such and SHOULD take the minimum over a short window as the link RTT.

### 6.9 Speed test (0x13, 0x14, 0x15 → 0x08)

Measures sender → receiver throughput (the video direction) on the actual
connection, so the user can check whether a Wi-Fi, Ethernet or Thunderbolt
link can carry the chosen bitrate, and so a sender can pick a bitrate
automatically. Only sent to a receiver with `caps` bit 2 set.

```
SPEED_TEST_START (0x13), sender → receiver, 12 bytes
0       u32   test_id       sender-chosen, non-zero, MUST differ from the
                            previous test on this connection (start at 1)
4       u32   duration_ms   planned send duration, 100 ≤ duration_ms ≤ 10000
8       u32   reserved      MUST be 0

SPEED_TEST_DATA (0x14), sender → receiver, 8 + N bytes
0       u32   test_id
4       u32   reserved      MUST be 0
8       ...   filler        N ≥ 0 bytes of arbitrary content; receiver
                            discards it. Recommended N: 256 KiB–1 MiB.

SPEED_TEST_END (0x15), sender → receiver, 12 bytes
0       u32   test_id
4       u64   bytes_sent    total filler bytes sent in SPEED_TEST_DATA for
                            this test (informational)

SPEED_TEST_RESULT (0x08), receiver → sender, 24 bytes
0       u32   test_id
4       u32   status        0 = OK, 1 = ABORTED (see below)
8       u64   bytes_received  total filler bytes received for this test
16      u64   elapsed_us    receiver clock, from the moment SPEED_TEST_START
                            was fully read to the moment SPEED_TEST_END was
                            fully read
```

Sender procedure:

1. Send SPEED_TEST_START.
2. Send SPEED_TEST_DATA messages back-to-back for `duration_ms` of wall time.
   Keep the send queue short (≤ 4 MiB outstanding) so SPEED_TEST_END is not
   stuck behind a large backlog when the time is up.
3. Send SPEED_TEST_END.
4. Wait for SPEED_TEST_RESULT (timeout: `duration_ms` + 5 s; on expiry treat
   the test as failed, keep the connection).
5. Throughput in kbit/s = `bytes_received × 8 × 1000 / elapsed_us`.
   The receiver-side measurement is authoritative: send-side timing only
   measures how fast the local socket buffer fills.

Rules:

- Only one speed test may be in progress per connection. While a test is in
  progress the sender MUST NOT send FRAME messages; it skips captures exactly
  as under backpressure (§8), so no encoded frame is ever dropped and no
  STREAM_STOP is needed. The receiver keeps showing the last picture.
  Control messages (HEARTBEAT, PING/PONG, KEYFRAME_REQUEST, GOODBYE) may flow
  as usual.
- A speed test MAY run in any receiver state after HELLO (HELLO_SENT,
  WAIT_KEYFRAME, STREAMING, PAUSED). The receiver's 5 s STREAM_START timer
  (§7.3) is suspended while a test is active and restarts from 5 s when the
  test ends (SPEED_TEST_END) or times out, so a sender can test the link
  before starting its first stream.
- The receiver counts filler bytes (payload length − 8) of every
  SPEED_TEST_DATA whose `test_id` matches the active test. SPEED_TEST_DATA or
  SPEED_TEST_END with no active test or a different `test_id` is ignored (and
  SHOULD be logged). A SPEED_TEST_START while a test is active aborts the old
  one: the receiver sends SPEED_TEST_RESULT for the old `test_id` with
  `status = 1` and starts the new one.
- If more than `duration_ms` + 5 s pass after SPEED_TEST_START without
  SPEED_TEST_END, the receiver sends SPEED_TEST_RESULT with `status = 1` for
  that test and forgets it.
- Receivers MUST read and discard filler without buffering it whole; they
  MUST NOT allocate per-message buffers proportional to `length` for this
  message type.
- After a test, a sender that was streaming resumes capture; the next FRAME is
  a normal delta frame (the receiver still holds the reference picture).
- Short tests under-report on links with a large bandwidth-delay product
  (TCP slow start). Senders SHOULD use ≥ 1000 ms for a user-requested test;
  500 ms is acceptable for a quick automatic check at connect time.

---

## 7. Connection lifecycle

### 7.1 Receiver state machine

```
LISTENING ──accept──▶ HELLO_SENT ──STREAM_START──▶ WAIT_KEYFRAME ──keyframe──▶ STREAMING
    ▲                     │                               │                      │
    │                     │ 5 s without STREAM_START      │ (KEYFRAME_REQUEST    │ STREAM_STOP
    │                     │ → GOODBYE TIMEOUT             │  after 500 ms)       ▼
    │                     ▼                               │                   PAUSED ──STREAM_START──▶ WAIT_KEYFRAME
    └───── close / peer dead / GOODBYE ◀─────────────────┴──────────────────────┘
```

- On accept: if a connection is already in any state other than LISTENING,
  reply GOODBYE `BUSY` and close the new one (v1 is single-source).
- On accept: send HELLO immediately, start the liveness timer.
- On any disconnect: stop decoding, blank the panel, return to LISTENING.
  The receiver MUST be ready to accept again within 1 s.

### 7.2 Sender state machine

```
IDLE ──connect──▶ WAIT_HELLO ──HELLO ok──▶ DISPLAY_UP ──STREAM_START sent──▶ STREAMING
  ▲                   │                        │                                 │
  │                   │ 3 s without HELLO      │ create virtual display,         │ KEYFRAME_REQUEST
  │                   │ → close, retry         │ start capture + encoder         │ → force keyframe
  │                   ▼                        │                                 │
  └──── disconnect / peer dead / GOODBYE ◀─────┴─────────────────────────────────┘
          │
          └─ release virtual display (windows return to the built-in screen),
             then retry connect with backoff: 1 s, 2 s, 4 s, ... max 10 s
```

- The sender MUST NOT create the virtual display before a valid HELLO; the
  display's geometry comes from HELLO.
- On disconnect the sender MUST release the virtual display within 2 s so the
  user's windows come back promptly.
- On resolution change of the virtual display (user picks a different mode in
  System Settings), the sender MUST send STREAM_STOP, STREAM_START, keyframe.

### 7.3 Handshake timing

| step                                   | timeout | on expiry                          |
|----------------------------------------|---------|------------------------------------|
| sender waits for HELLO after connect   | 3 s     | sender closes, retries             |
| receiver waits for STREAM_START        | 5 s (suspended during a speed test) | receiver sends GOODBYE TIMEOUT |
| sender waits for SPEED_TEST_RESULT     | duration + 5 s | test failed, connection kept |
| receiver waits for first keyframe      | 500 ms  | receiver sends KEYFRAME_REQUEST    |
| liveness                               | 2 s     | close, treat peer as dead          |

---

## 8. Flow control and latency

- There is no explicit flow control. TCP backpressure is the mechanism.
- Receiver: MUST NOT drop *encoded* frames (dropping a P-frame corrupts the
  picture until the next keyframe). Instead it stops reading the socket when
  its decode queue is full, which stalls the sender's writes. It MAY drop
  *decoded* frames to always show the newest one.
- Sender: when a FRAME write would block (or its outgoing queue holds more
  than 2 encoded frames), it MUST skip **capturing/encoding** new frames
  rather than discarding already-encoded ones. Skipping before the encoder is
  safe: the next encoded frame simply references the last one that was sent.
- Sender SHOULD only encode when the captured content changed (ScreenCaptureKit
  delivers frames only on change); an idle desktop produces no FRAMEs, only
  HEARTBEATs.
- Target end-to-end latency (capture to photons) is ≤ 50 ms at 60 Hz on a
  wired link. Encoders MUST run in real-time/low-latency mode with frame
  reordering disabled.

---

## 9. Error handling

A **protocol error** is any of: `length` > 64 MiB; a fixed-size payload with
the wrong length; a `string` extending past the payload; FRAME before
STREAM_START; STREAM_START geometry larger than the panel; SPEED_TEST_START
with `duration_ms` outside 100–10000 or `test_id` 0; `protocol_version`
≠ 1 (→ VERSION_MISMATCH instead); `hflags` or reserved fields non-zero MAY be
tolerated (ignore) but SHOULD be logged.

On a protocol error the detecting side MUST send GOODBYE `PROTOCOL_ERROR`
(best effort; the socket may already be broken) and close.

Decoder errors on the receiver: send KEYFRAME_REQUEST (rate-limited) and keep
going; if three consecutive keyframes fail to decode, GOODBYE
`UNSUPPORTED_CODEC`.

---

## 10. Security

v1 has **no authentication or encryption**. It is intended for a
point-to-point Thunderbolt or Ethernet link, or a trusted LAN. Anyone who can
reach the port can display arbitrary video on the receiver's panel. Receivers
SHOULD log the peer address of each connection. Operators who expose the port
on a shared network SHOULD firewall it to the sender's address.

Reserved for v2: TLS with pinned certificates (or a pre-shared key) on the same
port, negotiated before HELLO.

---

## 11. Conformance checklist

An implementation claiming IDP v1 conformance passes all of:

**Framing**
- [ ] Parses and emits the 8-byte header; rejects `length` > 64 MiB.
- [ ] Skips unknown message types without desync.
- [ ] Round-trips `protocol/fixtures/hello_imac.bin` byte-for-byte.

**Receiver**
- [ ] Sends HELLO before reading; `display_id` stable across restarts.
- [ ] Replies GOODBYE BUSY to a second concurrent connection.
- [ ] Ignores FRAMEs until the first keyframe after STREAM_START.
- [ ] Sends KEYFRAME_REQUEST ≤ 1/s; sends HEARTBEAT when idle; detects a dead
      peer in 2 s and returns to LISTENING within 1 s.
- [ ] Handles STREAM_STOP → STREAM_START with a new geometry without restart.
- [ ] Answers PING with a matching PONG.
- [ ] With `caps` bit 2: answers a speed test with SPEED_TEST_RESULT, discards
      filler without whole-message buffering, aborts a stale test.

**Sender**
- [ ] Waits for HELLO before creating the virtual display; releases it ≤ 2 s
      after disconnect.
- [ ] First FRAME after STREAM_START is a keyframe with VPS/SPS/PPS in-band,
      Annex-B, Main 4:2:0 8-bit, no reordering, strictly increasing `pts_us`.
- [ ] Honors KEYFRAME_REQUEST on the very next FRAME.
- [ ] Never drops an encoded frame; skips capture under backpressure.
- [ ] Sends HEARTBEAT at least every 500 ms when no FRAMEs flow.
- [ ] Reconnects with 1–10 s backoff.
- [ ] Answers PING with a matching PONG.
- [ ] Sends no FRAME between SPEED_TEST_START and SPEED_TEST_RESULT (or its
      timeout); computes throughput from the receiver's numbers.

---

## 12. Test fixtures

Directory `protocol/fixtures/` holds raw message bytes (header + payload),
generated by the Rust `protocol` module's `gen_fixtures` test and committed.
Both the Rust and Swift test suites MUST decode every fixture and re-encode it
to identical bytes.

| file                      | contents                                      |
|---------------------------|-----------------------------------------------|
| hello_imac.bin            | the HELLO from §6.1.2                         |
| stream_start_5k.bin       | STREAM_START 5120×2880, 60 Hz, hidpi, id 1     |
| frame_keyframe_min.bin    | FRAME, keyframe flag, pts 1 000 000, 8-byte dummy AU |
| frame_delta_min.bin       | FRAME, no flags, pts 1 016 667, 4-byte dummy AU |
| keyframe_request.bin      | KEYFRAME_REQUEST, empty payload               |
| heartbeat.bin             | HEARTBEAT mono_us = 123 456 789               |
| goodbye_busy.bin          | GOODBYE reason 2, message "busy"              |
| stream_stop_1.bin         | STREAM_STOP stream_id 1                       |
| stats.bin                 | STATS stream 1, received 600, decoded 598, dropped 2, avg_decode 3500 us, queue 1 |
| ping.bin                  | PING seq 7, t_us 5 000 000                    |
| pong.bin                  | PONG seq 7, t_us 5 000 000                    |
| speed_test_start.bin      | SPEED_TEST_START test 1, 1000 ms              |
| speed_test_data_min.bin   | SPEED_TEST_DATA test 1, 16 filler bytes 0xA5  |
| speed_test_end.bin        | SPEED_TEST_END test 1, bytes_sent 125 000 000 |
| speed_test_result.bin     | SPEED_TEST_RESULT test 1, OK, 125 000 000 bytes, 1 000 000 us (= 1 Gbit/s) |

The dummy AUs in fixtures are not valid HEVC; framing tests only.

Since the receiver and sender live in separate repositories, each carries a
copy of this file and of `protocol/fixtures/`. The receiver repository is the
source of truth for fixtures; the sender repository's
`scripts/sync-protocol.sh` copies them over, and both test suites check the
same bytes.

---

## 13. Changelog

- v1 (2026-11-01): initial version. Replaces the pre-protocol 13-byte frame
  header used by the prototype receiver (`u32 len, u64 pts, u8 flags`) and the
  bare `'K'` keyframe byte. Same day, before any release: added PING/PONG
  (§6.8), the speed test (§6.9, `caps` bit 2), an exact STATS length, and
  their fixtures. Additive only; no layout of an existing message changed.
