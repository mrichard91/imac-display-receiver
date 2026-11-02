# iMac Display Protocol (IDP) — version 1

Status: normative. This document is the single source of truth for everything
that crosses the wire between a **sender** (the machine whose desktop is being
extended, e.g. a MacBook) and a **receiver** (the machine whose panel shows the
picture, e.g. an iMac running Ubuntu). It is written so that third parties can
build their own senders and receivers and extend the protocol (§3.1–§3.4).

> **Security warning.** IDP v1 has **no authentication and no encryption**.
> The sender's screen crosses the network as a plain HEVC stream that anyone on
> the path can record and play back. Anyone who can reach the receiver's port
> can show video on its panel, occupy it, or feed its native video decoder
> untrusted input. Use IDP only on a point-to-point cable or a LAN where every
> host is trusted, with the port firewalled to known senders. **Never** use it
> on public or shared Wi-Fi or expose it to the internet. See §10.

If code and this document disagree, the code is wrong. Any change to the wire
format follows §3: additive changes as allowed by §3.1, everything else only
with a protocol version bump. Either way this file changes in the same pull
request.

Terminology: MUST, MUST NOT, SHOULD, MAY are used as in RFC 2119.

---

## 1. Overview

```
 sender (e.g. Mac)                              receiver (e.g. Linux)
 ─────────────────                              ─────────────────────
 creates virtual display  ──── TCP connect ────▶ listening on [::]:7878
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

- Roles: a receiver is any machine with a panel and an HEVC decoder; a sender
  is any machine that can present a display surface and encode it as HEVC.
  The protocol name comes from its first deployment, a 5K iMac running Ubuntu
  as a second display for a Mac, which this document uses as its running
  example. The reference implementations are a Rust/GStreamer receiver for
  Linux and a Swift menu bar sender for macOS; nothing on the wire depends on
  either platform.
- Transport: one TCP connection, opened by the sender. The receiver listens on
  one configurable port (default **7878**). The reference receiver listens on
  **all interfaces**, IPv4 and IPv6 (dual-stack `[::]`, falling back to
  `0.0.0.0`); see §10 for what that means for exposure. The sender is given the
  receiver's address, so the same binaries work over a Thunderbolt bridge,
  Ethernet, or any other IP link.
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
  An encoder MUST emit valid UTF-8; if it has to shorten text to fit 65 535
  bytes it MUST cut at a code point boundary. Received strings are untrusted
  display text (§10.1); see §3.1 for invalid UTF-8.
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
- There is no version negotiation. The receiver commits to its version in
  HELLO before it has read anything from the sender, and a v1 sender accepts
  only `protocol_version = 1`. Before STREAM_START the receiver does not know
  the sender's version at all and handles PING, HEARTBEAT, speed test and
  unknown messages from a peer of any version normally.

### 3.1 Compatibility rules

This subsection states exactly what a v1 peer does with input it does not
know. Both reference implementations behave as described. A new
implementation MUST behave the same way wherever a row states what a v1 peer
does, because extensions (§3.3) are designed against these rules; statements
about what "the reference receiver" or "the reference sender" does, and
logging details, are informative.

#### 3.1.1 Invariants for every future version

These parts of the wire format MUST NOT change in any version, so that a v1
peer can always detect a newer peer and say so:

- The 8-byte header (§4), including `type` at offset 0 and `length` at offset 4.
- HELLO is type 0x01 and its first `u16` is `protocol_version`. A v1 sender
  reads that `u16` before applying the rest of the v1 layout, sends GOODBYE
  `VERSION_MISMATCH` if it is not 1, and stops reconnecting.
- STREAM_START is type 0x10 with `protocol_version` as its first `u16`. A v1
  receiver checks the payload length (exactly 20 bytes) *before* it reads the
  version, so a later version that changes the STREAM_START length gets GOODBYE
  `PROTOCOL_ERROR` from a v1 receiver, not `VERSION_MISMATCH`. A version that
  wants a clean mismatch report from v1 receivers MUST keep STREAM_START at 20
  bytes and carry new data in other messages.
- GOODBYE is type 0x05 with layout `u16 reason, string message` and nothing
  after the string, and reason 1 means `VERSION_MISMATCH`. Otherwise a v1 peer
  reports `PROTOCOL_ERROR` instead and, if it is a sender, keeps retrying.

#### 3.1.2 What a v1 peer does with unknown or unexpected input

| input | v1 behaviour (both reference implementations unless stated) |
|-------|-------------------------------------------------------------|
| Unknown message type, `length` ≤ 64 MiB | Skipped: `length` bytes are read and discarded without allocating in proportion to `length`. Accepted in every state, never an error. Counts as liveness (§6.5). Does **not** restart the receiver's STREAM_START timer: a sender that sends only unknown types after HELLO gets GOODBYE `TIMEOUT` after 5 s. Receiver logs at debug level; sender does not log. |
| Any message type with `length` > 64 MiB | Protocol error. The length is checked before the type, so this applies to unknown types too. |
| A message that takes > 2 s to arrive | The reference receiver counts liveness per *completed* message and declares the peer dead mid-message (close, no GOODBYE). The reference sender counts bytes. Every message, of any type, MUST be deliverable well within 2 s on the slowest link it is used on (a 64 MiB message needs about 270 Mbit/s). |
| Known type, payload length not as in the table below | Protocol error (GOODBYE `PROTOCOL_ERROR`, close). No v1 message accepts trailing bytes. |
| Known control type (every known type the receiver buffers whole: in v1 all except FRAME and SPEED_TEST_DATA) longer than 65 573 bytes | The reference receiver rejects it as a protocol error before reading the payload. 65 573 = 38 + 65 535, the largest possible v1 HELLO. Other receivers MAY apply the same cap. |
| Known type sent in the wrong direction (e.g. HELLO from a sender, FRAME from a receiver) | Fully decoded with the normal length rules (a malformed one is a protocol error), then ignored and logged. The reference sender also version-checks a second HELLO, so a second HELLO with another `protocol_version` gets `VERSION_MISMATCH`. |
| `hflags` or header `reserved` non-zero | Ignored; logged (receiver: once per connection and type; sender: first time, then every 1000th). Encoders always write zeros. |
| Reserved payload fields non-zero (FRAME bytes 9–11, SPEED_TEST_START bytes 8–11, SPEED_TEST_DATA bytes 4–7) | Ignored. The reference receiver logs once per connection and type. |
| Unknown HELLO `caps` bits | Ignored; the reference sender logs a warning. |
| Unknown HELLO `codecs` bits | Ignored silently. If bit 0 is **clear**, a v1 sender sends GOODBYE `UNSUPPORTED_CODEC` and stops reconnecting, whatever other bits are set. |
| Unknown STREAM_START `sflags` bits | Ignored; the reference receiver logs once per connection. |
| Unknown FRAME `fflags` bits | Ignored; the reference receiver logs once per connection. The AU is decoded as if the bits were clear. |
| STREAM_START `codec` ≠ 1 | GOODBYE `UNSUPPORTED_CODEC`, close. Checked after `protocol_version`, before geometry. |
| Value outside a v1 range: STREAM_START width/height 0 or larger than the panel; SPEED_TEST_START `test_id` 0 or `duration_ms` outside 100–10000 | Protocol error, close. |
| Unknown GOODBYE `reason` | Treated as a generic close: logged, connection closed, nothing sent back. The reference sender reconnects with backoff after any reason except 1 (`VERSION_MISMATCH`) and 3 (`UNSUPPORTED_CODEC`). |
| Unknown SPEED_TEST_RESULT `status` | The reference sender treats every non-zero status as a failed test (and `elapsed_us = 0` as well). The connection is kept. |
| Invalid UTF-8 in a received `string` | The reference receiver treats it as a protocol error. The reference sender replaces invalid sequences with U+FFFD. Both are conforming; senders and receivers MUST NOT rely on either. |

Payload length rules for the v1 types. "Exact" means any other length is a
protocol error; "minimum" means shorter is a protocol error.

| type | rule |
|------|------|
| KEYFRAME_REQUEST | exact 0 |
| HEARTBEAT | exact 8 |
| STATS | exact 36 |
| PING, PONG | exact 12 |
| SPEED_TEST_RESULT | exact 24 |
| STREAM_START | exact 20 |
| STREAM_STOP | exact 4 |
| SPEED_TEST_START, SPEED_TEST_END | exact 12 |
| HELLO | minimum 38; the `name` string MUST end exactly at the end of the payload |
| GOODBYE | minimum 4; the `message` string MUST end exactly at the end of the payload |
| FRAME | minimum 12; everything after byte 12 is the access unit |
| SPEED_TEST_DATA | minimum 8; everything after byte 8 is filler |

Consequences:

- **No field can be appended to an existing message.** For the exact and
  string-terminated types a v1 peer rejects the longer payload; for FRAME and
  SPEED_TEST_DATA it silently reads the new field as video or filler.
- A type code's direction and layout are fixed for good. Reusing a code in the
  other direction or with another layout breaks v1 peers, which still decode
  wrong-direction messages strictly.
- `hflags` and the reserved fields are not usable extension points in v1:
  peers ignore them and log their use, and the reference encoders zero them on
  re-encode. They are kept for a future version.

#### 3.1.3 Safe without a version bump, and what needs one

Additive, no version bump (each still needs the process in §3.3):

| change | condition |
|--------|-----------|
| New sender → receiver message type | Sent only to a receiver that advertises a new HELLO `caps` bit for it. (An old receiver would skip it, but the sender could not tell.) |
| New receiver → sender message type | MUST be safe for a v1 sender to ignore: v1 senders drop unknown types silently and the receiver gets no acknowledgement. See §3.1.4. |
| New HELLO `caps` bit | Always safe; v1 senders ignore it. |
| New `codecs` bit | Bit 0 MUST stay set while v1 senders are to be served. |
| New STREAM_START `codec` value | Sent only to a receiver that advertises the matching `codecs` bit; a v1 receiver closes with `UNSUPPORTED_CODEC`. |
| New STREAM_START `sflags` bit | Informational, or with a meaning only when sent to a receiver that advertises a new `caps` bit for it; a v1 receiver ignores it. |
| New FRAME `fflags` bit | Only if a receiver that ignores the bit still decodes and shows the AU correctly. |
| Wider range for a range-checked field (e.g. a longer speed test) | Used only with a receiver that advertises a new `caps` bit for it. |
| New GOODBYE `reason` | v1 peers treat it as a generic close, and v1 senders retry after it; a reason meaning "do not retry" is not honoured by v1 senders. |
| New SPEED_TEST_RESULT `status` | v1 senders treat it as a failed test. |
| New behaviour inside an existing message that a v1 peer may ignore | Same rule as the bit or type that signals it. |
| Parameters for a `caps`-gated feature | Not in HELLO, which cannot grow. The receiver sets the `caps` bit and sends a new receiver → sender message with the parameters immediately after HELLO; a sender that knows the bit waits for that message before using the feature, and a v1 sender skips it. Use one `caps` bit per feature, not per parameter; the `caps` field has only 11 standard bits left. |
| Additional connection to the same port (for example a separate audio channel) | Not possible in v1: a v1 receiver answers every second connection, even from the same sender, with GOODBYE `BUSY` (§7.1). Use a different port, advertised by a `caps` bit, or wait for a version that defines multiple channels. |

Requires a version bump (§3.3): changing the length, layout or meaning of any
existing message, field, value or bit; appending fields; giving `hflags` or a
reserved field a meaning; making an optional message or behaviour mandatory;
removing a message type; and anything that violates §3.1.1.

A version bump is a flag day for receivers: a receiver that sends HELLO with
`protocol_version` ≠ 1 is refused by every v1 sender, which then stops
reconnecting. A receiver that must keep serving v1 senders MUST keep sending a
v1 HELLO and offer the newer version through a HELLO `caps` bit plus an
upgrade message (as sketched for TLS in §10.3), or on a different port.

#### 3.1.4 No sender capabilities in v1

A v1 sender advertises nothing about its optional features. The only
information it sends about itself is in STREAM_START (`protocol_version`,
`codec`, `sflags` bit 0), and that arrives late, possibly after a speed test.
A receiver can learn what a sender supports only by watching what it sends:
a sender that never starts a speed test looks the same as one that cannot.

Therefore, until a sender capability message is specified:

- an extension that adds receiver → sender messages or behaviour MUST be safe
  for a v1 sender to ignore, and the receiver MUST NOT depend on a response;
- an extension that needs the receiver to know what the sender supports MUST
  wait for a sender capability message.

Type 0x17 is reserved for that message, `SENDER_CAPS` (sender → receiver,
planned; see PLAN.md). Its layout is **not** specified. Implementations MUST
NOT send 0x17 until it is, and MUST skip it like any unknown type. Type 0x16
is reserved and never assigned, because it is the first byte of a TLS
handshake record (§10.3).

### 3.2 Registries

Values are assigned only by changing this document (§3.3). "Unassigned"
values MUST NOT be sent until assigned. "Experimental/private" values are
never assigned by this document; they are for local experiments, a peer that
does not know them skips or ignores them like any unknown value, and two
experiments that collide on a value will misread each other. A published
implementation SHOULD get a standard assignment instead.

#### 3.2.1 Message types

| type | name | direction | status |
|------|------|-----------|--------|
| 0x00 | — | — | reserved, never assigned |
| 0x01 | HELLO | receiver → sender | assigned (§6.1) |
| 0x02 | KEYFRAME_REQUEST | receiver → sender | assigned (§6.4) |
| 0x03 | HEARTBEAT | both | assigned (§6.5) |
| 0x04 | STATS | receiver → sender | assigned (§6.7) |
| 0x05 | GOODBYE | both | assigned (§6.6) |
| 0x06 | PING | both | assigned (§6.8) |
| 0x07 | PONG | both | assigned (§6.8) |
| 0x08 | SPEED_TEST_RESULT | receiver → sender | assigned (§6.9) |
| 0x09–0x0F | — | receiver → sender, or both | unassigned |
| 0x10 | STREAM_START | sender → receiver | assigned (§6.3) |
| 0x11 | FRAME | sender → receiver | assigned (§6.2) |
| 0x12 | STREAM_STOP | sender → receiver | assigned (§6.3) |
| 0x13 | SPEED_TEST_START | sender → receiver | assigned (§6.9) |
| 0x14 | SPEED_TEST_DATA | sender → receiver | assigned (§6.9) |
| 0x15 | SPEED_TEST_END | sender → receiver | assigned (§6.9) |
| 0x16 | — | — | reserved, never assigned (first byte of a TLS handshake record, §10.3) |
| 0x17 | SENDER_CAPS | sender → receiver | reserved: planned, layout not specified (§3.1.4, PLAN.md) |
| 0x18–0x1F | — | sender → receiver | unassigned |
| 0x20–0xDF | — | stated per type when assigned | unassigned, for future standard types |
| 0xE0–0xFE | — | any | experimental/private use |
| 0xFF | — | — | reserved, never assigned |

New standard types SHOULD be taken from the block for their direction
(0x09–0x0F, 0x18–0x1F) before 0x20–0xDF.

#### 3.2.2 Flag and value registries

| registry | assigned | unassigned | experimental/private |
|----------|----------|------------|----------------------|
| HELLO `caps` (u16, §6.1) | bit 0 can scale; bit 1 STATS; bit 2 speed test | bits 3–13 | bits 14–15 |
| HELLO `codecs` (u32, §6.1) | bit 0 HEVC Main 4:2:0 8-bit (MUST stay set, §3.1.2) | bits 1–27 | bits 28–31 |
| STREAM_START `codec` (u8, §6.3) | 1 = HEVC Main 4:2:0 8-bit | 2–223 | 224–255 |
| STREAM_START `sflags` (u8, §6.3) | bit 0 hidpi (informational) | bits 1–7 (informational, or `caps`-gated, §3.1.3) | — |
| FRAME `fflags` (u8, §6.2) | bit 0 keyframe | bits 1–7 | — |
| GOODBYE `reason` (u16, §6.6) | 0–6 | 7–0xEFFF | 0xF000–0xFFFF |
| SPEED_TEST_RESULT `status` (u32, §6.9) | 0 OK; 1 ABORTED | 2–0xEFFFFFFF | 0xF0000000–0xFFFFFFFF |
| Header `hflags`, header `reserved`, payload reserved fields | none | reserved for a future version (§3.1.2) | — |

STREAM_START `codec` value 0 is invalid. `codecs` is a bitmask and `codec` an
enumeration; v1 defines no general mapping between them beyond bit 0 ↔ value 1.
New assignments SHOULD pair `codecs` bit *n* with `codec` value *n* + 1 and
MUST state the pairing.

HELLO `codecs` and STREAM_START `codec` describe the video stream only. Every
`codecs` bit is a video codec that can be selected in STREAM_START. An
extension carrying other media (for example audio) defines its own codec field
and registry in its own messages and gates them with a HELLO `caps` bit.

### 3.3 How to propose an extension

The receiver repository is the source of truth for this file, PLAN.md and
`protocol/fixtures/`. The sender repository carries copies that are only ever
updated by its `scripts/sync-protocol.sh`; never edit them there.

1. Open a pull request against the receiver repository that explains the
   need, picks an extension point from §3.1.3, and says why a v1 peer stays
   correct when it meets the new traffic (§3.1.2).
2. In the same pull request, change PROTOCOL.md:
   - add the new message or field as a **new** subsection (for example §6.10);
     never renumber or retitle an existing section, because code in both
     repositories cites section numbers;
   - update the §5 table, the §3.2 registries, the §3.1.2 payload length
     table, the field descriptions of any message whose bits or values gain a
     meaning (for example HELLO `caps` in §6.1), the §7.3 timing table for new
     timers, and §7.1/§7.2 if connection states change (edit existing sections
     in place, but never renumber or retitle them);
   - gate the feature: a sender → receiver message (including the sender's
     half of a "both" type) is sent only to a receiver that advertises a new
     HELLO `caps` bit; a receiver → sender message (including the receiver's
     half of a "both" type) MUST be ignorable by a v1 sender (§3.1.4). A
     receiver → sender feature MAY also get a `caps` bit so that senders know
     to expect it;
   - state the security impact under §10: what an attacker who can reach the
     port, or who answers in place of the real receiver, gains from the new
     message. An extension that lets the peer act on the other machine (input
     injection, clipboard, file transfer, power or brightness control, audio
     capture) MUST NOT be enabled on a v1 (unauthenticated) connection unless
     the user explicitly turns it on for that peer, and MUST be off by default;
   - classify the type as control (payload buffered whole; the reference
     receiver rejects any control payload over 65 573 bytes) or bulk (needs its
     own reader arm: buffered like FRAME or discarded like SPEED_TEST_DATA),
     and state its maximum length;
   - state the payload length rule (exact or minimum), all reserved fields
     (MUST be 0), state rules, timers, and interaction with liveness (§6.5),
     flow control (§8), the speed test (§6.9) and the STREAM_START timer
     (§7.3);
   - add the new error cases to §9 and items to the §11 checklist;
   - add fixtures to §12;
   - add a §13 changelog entry that says whether the change is additive or a
     version bump.
3. Implement it in the Rust receiver (`src/protocol.rs`: a `msg_type`
   constant, a `Message` variant, and arms in `msg_type()`, `name()`,
   `encode()`, `decode()`, plus any flag or reserved masks in
   `payload_has_reserved_bits`; `src/net.rs`: add a control type to
   `is_control()`, otherwise it is skipped as unknown, or give a bulk type its
   own arm in `FramedReader::read` (FRAME reads the payload into one buffer,
   SPEED_TEST_DATA discards it through a scratch buffer); `src/session.rs`:
   handle it in `on_message`).
4. Add fixtures. Add an entry per fixture to `fixture_messages()` in
   `src/protocol.rs` and its values to
   `fixtures_contain_the_values_in_protocol_section_12`. Run
   `cargo test gen_fixtures -- --ignored`: it writes every fixture to
   `$TMPDIR/idp-fixtures-<pid>/` and compares each with the committed copy in
   `protocol/fixtures/`. It never writes into `protocol/fixtures/`, and for a
   new fixture it fails because no committed copy exists yet; copy the new file
   from that temporary directory into `protocol/fixtures/` by hand and run it
   again. Then run `cargo test`, which requires every `.bin` file to be listed
   in `fixture_messages()`, none to decode as an unknown type, and every one to
   re-encode byte for byte. Existing fixture bytes MUST NOT change except in a
   version bump. Fixtures MUST have `hflags` and all reserved fields zero,
   because encoders write zeros.
5. Implement it in the Swift sender after running
   `scripts/sync-protocol.sh <path-to-receiver>` there (it copies PROTOCOL.md,
   PLAN.md and the fixtures and mirrors deletions): a `MessageType` case, a
   message struct with its size, `init(reader:)` and `write(to:)`, a `Message`
   case with `typeCode`, `encode` and `decode` branches, handling in
   `Connection.handle`, and the new names and values in
   `Tests/IDPProtocolTests/FixtureTests.swift` (`allFixturesPresent` lists
   every fixture explicitly). `swift test` fails until this is done; that is
   intentional, so the two codecs move in lockstep.
6. Both `cargo test` and `swift test` pass, and both implementations still pass
   §11, before the change is merged.

A version bump follows the same steps, also changes `PROTOCOL_VERSION`, keeps
§3.1.1, and gives new layouts new fixture names.

### 3.4 Worked example: the speed test

The speed test (§6.9) was added to v1 exactly this way and is the pattern to
copy.

- **Need.** Measure sender → receiver throughput on the real connection. This
  needs the receiver to count bytes and report back, so it is a two-sided
  feature.
- **Gate.** The receiver advertises HELLO `caps` bit 2. A sender starts a test
  only when the bit is set; the reference sender reports "unsupported"
  otherwise. A receiver without the feature never sees the messages.
- **Types.** Three sender → receiver types from the sender block (0x13
  SPEED_TEST_START, 0x14 SPEED_TEST_DATA, 0x15 SPEED_TEST_END) and one
  receiver → sender type (0x08 SPEED_TEST_RESULT) from the receiver block.
  SPEED_TEST_RESULT is only sent in reply to a START, so it never reaches a
  sender that did not ask; one that did not understand it would skip it.
- **Layouts.** START, END and RESULT have exact lengths (12, 12, 24 bytes),
  with explicit reserved fields that MUST be 0. DATA is the one bulk type: an
  8-byte prefix plus filler, read and discarded in a streaming arm of the
  receiver's reader rather than buffered (so it is not in `is_control()`), and
  recommended at 256 KiB–1 MiB per message so each one arrives well within
  the 2 s liveness window.
- **Interaction with existing rules.** No new connection state. FRAMEs pause
  during a test using the existing backpressure rule (§8). The receiver's
  STREAM_START timer is suspended during a test (§6.9, §7.3), so a sender can
  test before its first stream. Value ranges (`duration_ms` 100–10000,
  `test_id` ≠ 0) are protocol errors, so widening them later needs a new
  `caps` bit (§3.1.3).
- **Fixtures.** `speed_test_start.bin`, `speed_test_data_min.bin`,
  `speed_test_end.bin`, `speed_test_result.bin` were added to §12, to
  `fixture_messages()` and to the Swift fixture list.
- **Changelog.** §13 records it as additive, with no existing layout changed
  and no version bump.
- **Failure modes left for a v1 peer.** A receiver without bit 2 that is sent
  a test anyway skips the messages; the sender's result timeout (`duration_ms`
  + 5 s) fails the test and keeps the connection, but such a receiver does not
  suspend its STREAM_START timer and may close with GOODBYE `TIMEOUT`. This is
  why the sender MUST check the bit first.

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

Types not listed here are not assigned: 0x00, 0x16 (the first byte of a TLS
handshake record, §10.3) and 0xFF are reserved and never assigned, 0x17 is
reserved for a planned SENDER_CAPS message (§3.1.4), 0xE0–0xFE are for
experimental/private use, and 0x09–0x0F, 0x18–0x1F and 0x20–0xDF are
unassigned. §3.2.1 is the registry. Unknown types MUST be skipped (§3.1.2).

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
                                  bits 3–13 unassigned and bits 14–15
                                  experimental (§3.2.2); unassigned bits
                                  MUST be 0
4       u32   panel_width_px      native panel width in pixels, e.g. 5120
8       u32   panel_height_px     native panel height in pixels, e.g. 2880
12      u32   refresh_mhz         native refresh, e.g. 60000
16      u16   physical_width_mm   active area width, 0 if unknown
18      u16   physical_height_mm  active area height, 0 if unknown
20      u64   display_id          stable identity of this receiver (§6.1.1)
28      u32   codecs              bit 0: HEVC Main 4:2:0 8-bit. MUST be set in v1.
                                  bits 1–27 unassigned, 28–31 experimental
                                  (§3.2.2)
32      u32   max_bitrate_kbps    hint, 0 = no preference
36      string name               human-readable, e.g. "iMac"
```

Every field of HELLO is unauthenticated input to the sender (§10.1). Senders
SHOULD bound the panel geometry and refresh rate to values their display
system can handle before creating a display, and SHOULD treat `name` as
untrusted display text (strip control and bidirectional-override characters,
limit the displayed length).

#### 6.1.1 display_id

The sender uses `display_id` to derive the virtual display's vendor/product/
serial so that macOS remembers arrangement and resolution between sessions.
It MUST be stable across reboots of the receiver and SHOULD be unique per
receiver. Recommended derivation: first 8 bytes of SHA-256 over
`/etc/machine-id` + the DRM connector name (e.g. `eDP-1`). A sender MUST treat
it as opaque.

`display_id` is not a secret and not proof of identity: the receiver sends it
to every client that connects while no session is active, so it fingerprints
the machine and anyone can replay it in their own HELLO (§10.1).

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
                          bits 1–7 unassigned (§3.2.2), MUST be 0 until
                          assigned
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
each stream (§6.3) and MUST NOT compare it to their own clock. v1 does not
require the epoch to stay the same across streams, or `pts_us` to share a
clock with HEARTBEAT `mono_us` or PING `t_us`. The reference sender uses one
monotonic clock (macOS `CLOCK_UPTIME_RAW`) for all three for the whole
connection. An extension that needs to relate other timestamps to video (for
example audio sync) MUST require this of any sender that enables it.

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
                                 bits 1–7 unassigned (§3.2.2), MUST be 0 until
                                 assigned
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
  receiver with `caps` bit 0 set scales it to the panel, preserving the aspect
  ratio and centring it on black (letterbox or pillarbox; never stretched);
  otherwise it centres the image unscaled on black. An extension that reports
  panel positions to the sender (for example pointer or touch input) MUST
  express them in stream pixels (0 ≤ x < `width_px`, 0 ≤ y < `height_px`)
  after undoing this mapping, and MUST NOT report positions outside the
  picture. Larger than the panel, or a width or height of 0, is a protocol
  error.
- STREAM_START is validated in this order: `protocol_version` ≠ 1 →
  `VERSION_MISMATCH`; `codec` ≠ 1 → `UNSUPPORTED_CODEC`; geometry →
  `PROTOCOL_ERROR`. Each closes the connection. `fps_mhz` and `sflags` bit 0
  are informational. A reused `stream_id` is tolerated (logged) by the
  reference receiver.
- On STREAM_STOP: stop decoding, show black (or a "paused" indicator), keep
  the connection open. FRAMEs received after STREAM_STOP and before a new
  STREAM_START MUST be ignored. STREAM_STOP before any STREAM_START is a
  protocol error (wrong state). A STREAM_STOP whose `stream_id` does not match
  is logged and still pauses; a STREAM_STOP while already paused is ignored.

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

A peer MAY count liveness per completely received message (the reference
receiver does) or per byte (the reference sender does). Every message
therefore MUST arrive within well under 2000 ms on the link in use, or the
receiver may declare the peer dead in the middle of it (§3.1.2).

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

An unknown `reason` MUST be treated as a generic close. A sender SHOULD NOT
reconnect automatically after `VERSION_MISMATCH` or `UNSUPPORTED_CODEC`; the
reference sender retries with backoff (§7.2) after every other reason,
including `BUSY` and unknown values. `message` is free text from the peer:
the side that receives it MUST treat it as untrusted (§10.1), and the side
that sends it SHOULD NOT include local secrets or detailed internal state.

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
  as usual. Every extension message type MUST state whether it is sent during
  a speed test; bulk extension traffic SHOULD pause like FRAME, because its
  bytes are not counted and lower the measured throughput.
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
- All sender → receiver messages share one ordered TCP stream. While the
  receiver has stopped reading for FRAME backpressure, nothing queued behind
  that FRAME is read either, control messages and any extension traffic
  included. An extension that needs bounded latency independent of video (for
  example audio) MUST tolerate these stalls (buffering, concealment), or wait
  for a version that defines a separate channel; v1 has none (§7.1: a second
  connection gets BUSY).

---

## 9. Error handling

A **protocol error** is any of: `length` > 64 MiB; a fixed-size payload with
the wrong length; a `string` extending past the payload; FRAME before
STREAM_START; STREAM_START geometry larger than the panel; SPEED_TEST_START
with `duration_ms` outside 100–10000 or `test_id` 0; `protocol_version`
≠ 1 (→ VERSION_MISMATCH instead). Non-zero `hflags` or reserved fields are
not a protocol error: they are ignored (§2, §3.1.2) and SHOULD be logged.

Also protocol errors: STREAM_START width or height 0; STREAM_STOP before any
STREAM_START; bytes after the string in HELLO or GOODBYE (§3.1.2); a known
control message longer than 65 573 bytes, where a receiver applies that cap
(§3.1.2); invalid UTF-8 in a `string`, where a peer chooses to reject it
(§3.1.2). STREAM_START `codec` ≠ 1 is answered with GOODBYE
`UNSUPPORTED_CODEC` instead. The reference receiver tolerates and logs a reused
`stream_id`, a mismatched STREAM_STOP `stream_id`, stale SPEED_TEST_DATA/END
and well-formed wrong-direction messages, and silently ignores PONG (it sends
no PING).

On a protocol error the detecting side MUST send GOODBYE `PROTOCOL_ERROR`
(best effort; the socket may already be broken) and close.

Decoder errors on the receiver: send KEYFRAME_REQUEST (rate-limited) and keep
going; if three consecutive keyframes fail to decode, GOODBYE
`UNSUPPORTED_CODEC`.

---

## 10. Security

v1 has **no authentication, no integrity protection and no encryption**. It
is intended only for a point-to-point Thunderbolt or Ethernet link, or a LAN
on which every host is trusted (§10.2). Neither side can tell who it is
talking to. Receivers SHOULD log the peer address of each connection.

### 10.1 Threat model

An attacker is anyone who can see traffic on the path, reach the receiver's
port, or make the sender connect to them (wrong or reassigned address,
spoofed DNS or mDNS name, ARP/ND spoofing).

**Confidentiality of the sender's screen.** FRAME payloads are a plain
Annex-B HEVC stream. Anyone who can capture the traffic (shared Wi-Fi, a LAN
segment, a mirror port, a compromised router, ARP/ND spoofing) can decode
everything shown on the extended display from the next keyframe on.
FRAME sizes and timing (HEARTBEATs fill the gaps when the screen is static)
also reveal when the user is active, and would do so even under encryption.
This is the largest risk of v1.

**The sender trusts whoever answers.** A v1 sender streams to any host that
sends a well-formed HELLO. `name` and `display_id` are not authenticated, and
`display_id` is sent to every client that connects while no session is
active, so a spoofing receiver can copy the real one; a sender that derives
the virtual display's identity from it (§6.1.1) then gets the user's saved
arrangement back, and windows the user kept on that display move to the
attacker's display. A sender that reconnects
automatically to a host name, or to an address that is later given to another
machine, streams to whoever is there.

**Receiver exposure.** Anyone who can reach the port:

- receives HELLO before saying anything, whenever no session is active: the
  receiver's name (by default the hostname in the reference receiver), its
  stable `display_id`, panel geometry and physical size. While a session is
  active it gets GOODBYE `BUSY` instead, which tells a prober that a session
  is active;
- can show arbitrary video full screen on the panel;
- feeds attacker-chosen HEVC to the receiver's parser and decoder (in the
  reference receiver GStreamer's `h265parse`, then VA-API through Mesa and GPU
  firmware, or the libav software decoder), which are large native code bases
  outside the receiver's own memory safety. STREAM_START geometry does not
  bound what the bitstream's SPS asks the decoder to allocate. A decoder crash
  that restarts the service can be triggered again;
- can hold the single sender slot (§7.1) indefinitely: the slot is taken at
  accept, and the holder needs only to send any message every 2 s. PAUSED and
  WAIT_KEYFRAME have no deadline, and chained speed tests keep postponing the
  STREAM_START timer. The real sender gets `BUSY`;
- can make the receiver use memory and CPU: a FRAME header may announce up to
  64 MiB, which the reference receiver allocates before reading the payload
  and before checking the session state; several FRAMEs can be queued at
  once; each accepted connection gets a thread; each STREAM_START rebuilds the
  video pipeline; PINGs are answered without a rate limit.

**What the panel shows.** The reference receiver's idle screen draws the
receiver's name and every address a sender could type (all except loopback
and IPv6 link-local), with the port, on the panel while no sender is
connected, so anyone who can see the screen learns how to reach it.

**What a malicious receiver can do to a sender.** Besides receiving the
screen contents:

- every HELLO field is untrusted input. Panel geometry and refresh rate go to
  the sender's display system (the reference sender passes them to macOS
  without bounds checks); `name` can be up to 65 535 bytes of arbitrary
  Unicode and is shown in the sender's UI, stored and logged;
  `max_bitrate_kbps` can force very low quality;
- KEYFRAME_REQUEST faster than the 1/s limit forces repeated keyframes (CPU,
  bandwidth, battery); the reference sender does not rate-limit it. PINGs are
  answered without a limit;
- the reference sender buffers any known-type message up to the 64 MiB framing
  limit before checking its length;
- GOODBYE `message` text is shown to the user, and the sender's own GOODBYE
  `INTERNAL_ERROR` text may describe local errors to the receiver.

Requirements on every implementation, whatever the deployment:

- Treat every field and string from the peer as untrusted. Never let a length
  field cause an allocation above the 64 MiB framing limit; receivers MUST NOT
  buffer SPEED_TEST_DATA filler or unknown payloads whole (§3.1.2, §6.9).
- Implementations SHOULD cap control messages (65 573 bytes is the largest v1
  HELLO) before buffering them, SHOULD bound HELLO geometry before using it,
  and SHOULD rate-limit their responses to PING, KEYFRAME_REQUEST and
  STREAM_START.
- Receivers SHOULD log the peer address of every connection and every
  rejection.
- An extension that lets the peer act on the other machine (input injection,
  clipboard, file transfer, power or brightness control, audio capture) MUST
  NOT be enabled on a v1 (unauthenticated) connection unless the user
  explicitly turns it on for that peer, and MUST be off by default (§3.3).

### 10.2 Deployment requirements for v1

- Run IDP only on a point-to-point cable (Thunderbolt bridge, direct Ethernet)
  or on a LAN where every host and everyone with access to it is trusted.
- **Never** run it on public or shared Wi-Fi (cafés, hotels, offices, campus,
  conference networks), and never make the port reachable from the internet.
  Do not port-forward it, and check that the router does not pass inbound
  IPv6 to the receiver.
- Firewall the receiver's port to the known sender addresses, for IPv4 **and**
  IPv6. A cable alone is not enough: the reference receiver listens on every
  interface (dual-stack `[::]`) and has no option to bind one address, so any
  other interface that is up (Wi-Fi, LAN) also exposes the port.
- On the sender, connect by IP address on a link you control rather than by a
  host name that DNS or mDNS could redirect, and turn off automatic
  reconnection on networks you do not control. Keep sensitive windows off the
  extended display.
- On the receiver, set a name that is not the hostname where that matters,
  disable the idle screen if untrusted people can see the panel, keep the
  video stack (GStreamer, FFmpeg/libav, Mesa, kernel, GPU firmware) patched,
  and run the service unprivileged and as sandboxed as the platform allows
  (the reference systemd unit runs as a dedicated `imac-display` user, created
  at install, with basic hardening: `NoNewPrivileges`, `ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, `ProtectKernelTunables`,
  `ProtectControlGroups`, `RestrictAddressFamilies`; device, capability and
  syscall restrictions are not yet applied, see PLAN R10).

### 10.3 Reserved for v2

A future version is expected to add TLS with pinned certificates (or a
pre-shared key) on the same port, together with pairing. None of this is
specified yet; constraints a design MUST respect:

- The receiver speaks first (§6.1) and a v1 sender waits 3 s for HELLO, so a
  secure mode cannot simply start before HELLO without breaking v1 peers. A
  v1 receiver that is sent a TLS ClientHello reads its first 8 bytes as an
  IDP header of type 0x16, a type that is never assigned (§3.2.1). The
  `length` field is made of TLS length bytes and is usually above 64 MiB, so
  the receiver answers GOODBYE `PROTOCOL_ERROR` and closes; otherwise it
  skips the message as an unknown type (up to 64 MiB) and closes after the
  liveness timeout. Either way the TLS client receives an IDP HELLO instead of
  a ServerHello. Type 0x16 stays unassigned so that a future receiver can tell
  a TLS handshake from IDP by the first byte. Candidate approaches are a HELLO
  `caps` bit plus an upgrade message after which both sides start TLS on the
  same socket and the receiver repeats HELLO inside it, or a separate port.
- Pairing: the first connection is confirmed with a code shown on the
  receiver's panel. A short code MUST NOT be used directly as a TLS
  pre-shared key, because a recorded handshake lets an attacker try every
  code offline; it has to go through a PAKE (for example SPAKE2 or CPace) or
  be used only to confirm a pinned certificate fingerprint.
- Both sides authenticated, every message including HELLO confidential and
  integrity-protected, and no silent downgrade: once a sender has paired with
  a receiver, it MUST refuse plaintext from it.

### 10.4 Reporting vulnerabilities

Report vulnerabilities in the protocol or in either reference implementation
as described in `SECURITY.md` in the receiver and sender repositories. Do not
open a public issue for an unfixed vulnerability. A protocol-level fix is
proposed like any other change (§3.3).

---

## 11. Conformance checklist

An implementation claiming IDP v1 conformance passes all of:

**Framing**
- [ ] Parses and emits the 8-byte header; rejects `length` > 64 MiB.
- [ ] Skips unknown message types without desync.
- [ ] Round-trips `protocol/fixtures/hello_imac.bin` byte-for-byte.
- [ ] Applies the length rules and unknown-value handling of §3.1.2.

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
committed to the receiver repository. The Rust `protocol` module's
`gen_fixtures` test regenerates them into a temporary directory and checks
them against the committed copies; adding a fixture is a manual copy (§3.3,
step 4). Both the Rust and Swift test suites MUST decode every fixture and
re-encode it to identical bytes. A third-party implementation SHOULD do the
same with these files.

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
- v1 (2026-11-02): documentation only, no wire change; no message layout,
  value or fixture changed. Added §3.1 compatibility rules (as both reference
  implementations behave), §3.2 registries with ranges for future and
  experimental values, §3.3 extension process and §3.4 the speed test as a
  worked example. Recorded that v1 has no sender capabilities, reserved
  type 0x17 for a planned SENDER_CAPS (layout not specified), and marked
  type 0x16 as never assigned because it is the first byte of a TLS
  handshake record (§10.3). Rewrote §10 Security (§10.1–§10.4) and added a
  warning at the top. Added guidance for extension authors in §3.1.3,
  §3.2.2, §6.2 (`pts_us` clock), §6.3 (scaling and stream coordinates), §6.9
  and §8 (head-of-line blocking). Documented existing behaviour in §1
  (dual-stack listening, neutral roles), §6.1, §6.1.1, §6.3 (validation
  order, zero geometry, STREAM_STOP before STREAM_START), §6.5 (liveness
  during long messages), §6.6 (unknown reasons, retry), §9 (further protocol
  errors) and §12 (how fixtures are checked and added). Stated in §2 that an
  encoder truncates strings at a code point boundary; the reference sender
  cut at a byte boundary and was fixed to match (a sender bug fix, no wire
  format change).
