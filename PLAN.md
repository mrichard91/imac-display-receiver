# iMac Display — implementation plan

Turn an iMac running Ubuntu into an arrangeable extra display for a Mac, over
any IP link (Thunderbolt bridge, Ethernet, LAN). Two programs:

- **receiver** (Rust, Ubuntu): listens on all interfaces on one port, decodes
  HEVC with VA-API, shows it fullscreen on the panel.
- **sender** (Swift, macOS): a menu bar app installed on any number of Macs.
  From the menu the user connects to a receiver by IP/hostname and port,
  disconnects, sees status (state, link interface, RTT, effective bitrate,
  fps) and runs a link speed test. While connected it creates a virtual
  display matching the receiver's panel, captures it, encodes HEVC with
  VideoToolbox and streams it.

The wire format is fixed by `PROTOCOL.md`. Read it first; it is normative and
this plan does not repeat it.

---

## 0. Ground rules for everyone working on this

1. `PROTOCOL.md` wins. If you need a wire change, change the doc in the same PR
   and bump the version. Do not add "temporary" fields.
2. Each task below names the files it owns. Stay inside them; if you must touch
   another task's files, say so in the PR description.
3. Every PR that touches `protocol/` or the Swift `IDPProtocol` module must
   keep `protocol/fixtures/*.bin` round-tripping in both test suites.
4. Definition of done for any task: builds with no warnings, its acceptance
   checks pass, and the relevant conformance boxes in `PROTOCOL.md` §11 are
   ticked in the PR.
5. No private macOS API outside `sender/Sources/CGVirtualDisplayBridge/`.
   That directory is the one place expected to break on a macOS update.
6. Log with a level and a component prefix (`[net]`, `[gst]`, `[vt]`,
   `[display]`). Receiver logs go to stderr/journald; sender to stderr and
   `os_log`.
7. Rust: edition 2024, `cargo clippy -- -D warnings`, `cargo fmt`.
   Swift: 5.9+, SwiftPM only (no Xcode project), `swift build -c release`.

---

## 1. Repository layout

The two programs live in **two repositories** (both private on GitHub):
`imac-display-receiver` and `imac-display-sender`. Each carries an identical
copy of `PROTOCOL.md`, `PLAN.md` and `protocol/fixtures/`. The receiver repo is
the source of truth for fixtures; the sender repo's
`scripts/sync-protocol.sh <path-to-receiver-repo>` copies `PROTOCOL.md`,
`PLAN.md` and the fixtures over and fails if they then differ. A protocol
change is two PRs (receiver first, then sender sync) with the same version.

The trees below show each repository's root as `receiver/` and `sender/`.

```
imac-display-receiver/          (repo root = receiver/ below)
├── PROTOCOL.md, PLAN.md        copies, kept identical in both repos
├── README.md                   receiver quick start
├── protocol/
│   └── fixtures/*.bin          golden message bytes (source of truth)
├── receiver/                   Rust crate `imac-display-rx` (files at repo root)
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs             CLI, service loop
│   │   ├── protocol.rs         message types, encode/decode, fixtures test
│   │   ├── net.rs              listener, framed reader/writer, liveness timers
│   │   ├── session.rs          receiver state machine (PROTOCOL §7.1)
│   │   ├── pipeline.rs         GStreamer: appsrc → h265parse → VA → sink
│   │   └── panel.rs            DRM probe: native mode, size, display_id
│   ├── examples/
│   │   └── test_sender.rs      Linux stand-in for the Mac sender (x265)
│   ├── src/speedtest.rs        receiver side of PROTOCOL §6.9 (pure, testable)
│   ├── systemd/imac-display-rx.service
│   └── tests/integration.rs    receiver ⇄ test_sender over loopback, fakesink
└── docs/
    └── network-setup.md        Thunderbolt bridge / Ethernet / Wi-Fi addressing

imac-display-sender/            (repo root = sender/ below)
├── PROTOCOL.md, PLAN.md, protocol/fixtures/   synced copies
├── sender/                     Swift package `ImacDisplaySender` (files at repo root)
│   ├── Package.swift
│   ├── Sources/
│   │   ├── IDPProtocol/        message types, encode/decode (pure Swift, testable)
│   │   ├── CGVirtualDisplayBridge/  ObjC headers for the private API (only place)
│   │   ├── IDPCore/            everything testable without UI or private API
│   │   │   ├── Connection.swift    Network.framework TCP, framed I/O, liveness, PING
│   │   │   ├── SpeedTest.swift     sender side of PROTOCOL §6.9
│   │   │   ├── BitrateMeter.swift  sliding-window effective bitrate / fps
│   │   │   └── LinkInfo.swift      interface name + type (Wi-Fi/Ethernet/Thunderbolt)
│   │   └── ImacDisplaySender/      the menu bar app
│   │       ├── App.swift           @main SwiftUI App with MenuBarExtra
│   │       ├── MenuView.swift      status, stats, connect/disconnect, speed test
│   │       ├── ConnectView.swift   host + port entry, recent receivers
│   │       ├── SenderSession.swift state machine (PROTOCOL §7.2), ObservableObject
│   │       ├── VirtualDisplay.swift CGVirtualDisplay lifecycle
│   │       ├── Capture.swift       ScreenCaptureKit stream for one display ID
│   │       ├── Encoder.swift       VideoToolbox HEVC → Annex-B access units
│   │       └── Preferences.swift   recent hosts, bitrate mode, auto-reconnect
│   ├── scripts/make-app-bundle.sh, scripts/sync-protocol.sh
│   └── Tests/                  IDPProtocolTests (fixtures), IDPCoreTests
```

There is no prototype to migrate: the receiver is written from scratch
against `PROTOCOL.md`.

---

## 2. Architecture (one paragraph each)

**Receiver.** A single-threaded accept loop hands each connection to a
`session` that owns a GStreamer pipeline. The socket reader thread parses
framed messages; FRAME payloads are read straight into `gst::Buffer`s and
pushed to a blocking `appsrc` (backpressure = TCP backpressure). A timer thread
sends HEARTBEAT every 500 ms and enforces the 2 s liveness timeout. The
pipeline is `appsrc → h265parse → vah265dec → queue(leaky,1) → kmssink`, with
`avdec_h265` as a software fallback and `SINK` overridable for testing. Panel
geometry for HELLO comes from DRM sysfs at startup.

**Sender.** `main` resolves the target host (flag, else prompt with the
last-used value pre-filled), then runs `SenderSession`, which owns one
`Connection` and drives the §7.2 state machine. On HELLO it creates a
`VirtualDisplay` from the advertised geometry, starts `Capture` on that
display ID, and feeds frames to `Encoder`. Encoded access units are converted to
Annex-B (parameter sets prepended on keyframes) and written as FRAME messages.
Backpressure is observed on the connection's send queue depth: when >2 frames
are queued, incoming captures are skipped before encoding. Disconnect tears
everything down and schedules a reconnect.

---

## 3. Receiver tasks (Rust)

### R0 — Protocol module and fixtures
Owns: `receiver/src/protocol.rs`, `protocol/fixtures/`.
- Define `enum Message` with one variant per type in PROTOCOL §5 and
  `encode(&self, &mut Vec<u8>)` / `decode(type, payload) -> Result`.
- `FRAME` decode must be zero-copy friendly: expose `FrameHeader` (12 bytes)
  separately so `net.rs` can read the AU directly into a `gst::Buffer`.
- `#[test] #[ignore] gen_fixtures` writes every file in PROTOCOL §12 into
  `protocol/fixtures/`. A normal test reads each fixture, decodes, re-encodes,
  asserts byte equality. Verify `hello_imac.bin` equals the hex in §6.1.2.
- Unknown type → `Message::Unknown { type, len }` so callers can skip.
Accept: `cargo test` green; fixtures committed; hex in §6.1.2 matches.

### R1 — Framed I/O, session state machine, migrate prototype
Owns: `receiver/src/{net.rs,session.rs,main.rs}`.
- `net.rs`: `FramedReader` (BufReader, header parse, length guard, reads FRAME
  AUs into a caller-supplied `gst::Buffer`), `FramedWriter` behind a `Mutex`
  so the heartbeat thread and session can both write.
- `session.rs`: states per PROTOCOL §7.1; HELLO-first; STREAM_START
  (re)builds the pipeline via `pipeline.rs`; drops FRAMEs until keyframe;
  KEYFRAME_REQUEST at 500 ms after STREAM_START and on decoder error, rate
  limited to 1/s; STREAM_STOP blanks; GOODBYE BUSY for a second connection.
- Liveness: heartbeat thread, 500 ms send / 2000 ms timeout. Every received
  message resets the timer.
- `main.rs`: CLI (`--port 7878`, `--sink kmssink`, `--decoder auto|<element>`,
  `--log-level`); binds `0.0.0.0:<port>` (dual-stack `[::]` if available).
  Keep env var equivalents `PORT`, `SINK`, `DECODER` for systemd.
Accept: integration test (R4) passes; manual run with `--sink fakesink`
against `test_sender` logs HELLO → STREAM_START → keyframe → streaming.

### R2 — Pipeline hardening
Owns: `receiver/src/pipeline.rs`.
- Factor the prototype's pipeline build into `Pipeline::new(geometry, cfg)`.
- Decoder selection: `vah265dec` → `vaapih265dec` → `avdec_h265`; log choice;
  report `caps` bit 0 (can scale) only when the sink supports it.
- No `videoconvert` on the VA path (keeps DMABuf zero-copy); add it only for
  software decode.
- Sink config: `sync=false`; `kmssink` `force-modesetting=true` if present so
  it owns the CRTC; `fullscreen`/`can-scale` when available.
- Bus watcher: surface errors to `session` as `PipelineEvent::Error`, which
  triggers KEYFRAME_REQUEST or GOODBYE INTERNAL_ERROR after 3 failures.
- Pad probe on sink for frame counting; expose counters for STATS (optional,
  caps bit 1).
Accept: on the iMac, `GST_DEBUG=va*:4` shows `vah265dec` negotiating
`video/x-raw(memory:DMABuf)` or `VAMemory` straight into `kmssink`; CPU use at
5K60 < 25 % of one core.

### R3 — Panel probe and identity
Owns: `receiver/src/panel.rs`.
- Read `/sys/class/drm/card*-*/status` to find the connected internal connector
  (prefer `eDP-*`), parse `modes` for the native (first) mode, parse the EDID
  blob (`edid` file) for physical size in cm → mm. Fall back to env overrides
  `PANEL_WIDTH`, `PANEL_HEIGHT`, `PANEL_REFRESH_MHZ`, `PANEL_MM`.
- `display_id` = first 8 bytes of SHA-256(`/etc/machine-id` ‖ connector name).
- Fill HELLO from this; `name` from `/etc/hostname`.
Accept: unit test on a captured iMac sysfs snapshot (commit the snapshot under
`receiver/tests/data/`); on the real iMac HELLO reports 5120×2880 @ 60000.

### R4 — Integration test and test sender
Owns: `receiver/examples/test_sender.rs`, `receiver/tests/integration.rs`.
- Port `test_sender` to the protocol: wait for HELLO, send STREAM_START using
  HELLO geometry (or `--width/--height` smaller), x265 Main profile
  (`format=I420`), `h265parse config-interval=-1`, honor KEYFRAME_REQUEST via
  `UpstreamForceKeyUnitEvent` pushed on the appsink's sink pad, heartbeats.
- Integration test: spawn receiver with `--sink fakesink` on an ephemeral
  port, run test_sender for 3 s, assert ≥ 150 frames rendered, then reconnect
  at a different resolution and assert a second `first frame` log line; test
  BUSY on a second concurrent connection; test liveness timeout by pausing the
  sender for 3 s.
Accept: `cargo test --release` green in CI (software decoder).

### R5 — Service packaging
Owns: `receiver/systemd/`, `README.md` (receiver section), `docs/network-setup.md`.
- `imac-display-rx.service`: `After=network-online.target`, runs as a
  dedicated user in groups `video,render`, `Restart=always`,
  `Environment=SINK=kmssink`. Document that `kmssink` needs no display server
  running on that seat (disable GDM or run on a spare VT).
- `docs/network-setup.md`: Thunderbolt bridge addressing (Mac manual
  `10.99.0.1/30`, no router; Ubuntu `nmcli ... thunderbolt0 10.99.0.2/30`),
  Ethernet alternative, `boltctl` authorization, firewall note (PROTOCOL §10).
Accept: fresh Ubuntu 24.04 iMac follows README to a running service; `ss -ltn`
shows `0.0.0.0:7878`.

---

## 4. Sender tasks (Swift)

### S0 — IDPProtocol module
Owns: `sender/Sources/IDPProtocol/`, `sender/Tests/IDPProtocolTests/`.
- `enum Message` mirroring PROTOCOL §5; `encode() -> Data`;
  `static func decode(header:payload:) throws -> Message`; big-endian helpers.
- `FrameHeader` as a separate 12-byte struct so `Encoder` output can be sent
  with no copy of the AU (header `Data` + AU `Data` as two writes/`writev`).
- Tests load `../../protocol/fixtures/*.bin` (path relative to the package
  via `Bundle.module` resources or a symlink) and assert decode → encode
  identity.
Accept: `swift test` green; every fixture round-trips.

### S1 — Connection and liveness
Owns: `sender/Sources/ImacDisplaySender/Connection.swift`.
- `NWConnection` over TCP, `noDelay = true`, `serviceClass = .responsiveData`.
- Framed receive loop (read 8-byte header, then payload) delivering `Message`
  to a delegate on a dedicated serial queue.
- `send(_ message:)` with an outgoing queue; expose `pendingFrames` (count of
  FRAME messages not yet completed by `send(completion:)`) for backpressure.
- Heartbeat timer: send HEARTBEAT if nothing sent for 500 ms; disconnect if
  nothing received for 2000 ms.
Accept: unit test against a local `NWListener` echoing fixtures; heartbeat
observed with `tcpdump` on loopback.

### S2 — Virtual display
Owns: `sender/Sources/CGVirtualDisplayBridge/`, `VirtualDisplay.swift`.
- Bridging header declaring `CGVirtualDisplayDescriptor`, `CGVirtualDisplay`,
  `CGVirtualDisplaySettings`, `CGVirtualDisplayMode` (copy DeskPad's shapes;
  mark every selector as `@optional`-style lookups so a missing class fails
  gracefully with a clear error instead of crashing).
- `VirtualDisplay.create(from hello: Hello) throws -> VirtualDisplay`:
  `maxPixelsWide/High` = panel size; `sizeInMillimeters` from HELLO (fallback
  to 16:9 at 110 ppi if 0); `vendorID = 0x1D1D`, `productID` and `serialNum`
  derived from `display_id` so arrangement persists; `hiDPI = 1`; modes: the
  native mode plus the half-resolution "looks like" mode; `queue = .main`.
- Expose `displayID: CGDirectDisplayID`, `currentPixelSize`, and a callback
  when the mode changes (observe `CGDisplayRegisterReconfigurationCallback`
  for that ID) so the session can STREAM_STOP/START.
- `release()` drops the object; verify with `CGGetActiveDisplayList` that the
  ID disappears (may take up to ~2 s; do not block on it).
- Do not change the display's mode programmatically after creation (known
  to make the display un-removable).
Accept: running the step alone shows a new display in System Settings →
Displays with the right size, arrangeable; quitting removes it within 2 s.

### S3 — Capture
Owns: `Capture.swift`.
- `SCShareableContent.current`, pick `SCDisplay` with `displayID` from S2.
- `SCStreamConfiguration`: `width/height` = coded pixel size, `pixelFormat =
  kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` (NV12; VideoToolbox takes
  it without conversion), `minimumFrameInterval = 1/60`, `queueDepth = 3`,
  `showsCursor = true`, `capturesAudio = false`.
- Deliver `CMSampleBuffer`s on a dedicated queue; drop immediately if the
  session reports backpressure (PROTOCOL §8).
- Handle `SCStreamDelegate.stream(_:didStopWithError:)` by tearing down.
- Document the Screen Recording permission: the grant attaches to the
  launching app (Terminal when run from a shell). Provide
  `scripts/make-app-bundle.sh` in S6 so the permission attaches to the tool
  itself.
Accept: frame callbacks at ~60 Hz when content changes, ~0 when idle.

### S4 — Encoder
Owns: `Encoder.swift`.
- `VTCompressionSession` with `kCMVideoCodecType_HEVC`, encoder spec requiring
  hardware (`kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder`).
- Properties: `RealTime = true`, `ProfileLevel = HEVC_Main_AutoLevel`,
  `AllowFrameReordering = false`, `MaxKeyFrameInterval = Int32.max`,
  `MaxKeyFrameIntervalDuration = 0`, `AverageBitRate` (default 80 Mbps,
  flag `--bitrate`), `DataRateLimits` at 1.5× average per second,
  `PrioritizeEncodingSpeedOverQuality = true`, `MaxFrameDelayCount = 0`.
- `encode(sample, forceKeyframe: Bool)` sets
  `kVTEncodeFrameOptionKey_ForceKeyFrame` when requested.
- Output callback converts to Annex-B: walk the `CMBlockBuffer`, replace each
  4-byte length with `00 00 00 01`; for keyframes (no
  `kCMSampleAttachmentKey_NotSync`), prepend VPS/SPS/PPS from
  `CMVideoFormatDescriptionGetHEVCParameterSetAtIndex` (0,1,2), each with a
  start code. Set `pts_us` from the sample's presentation timestamp
  (`mach_absolute_time`-based, monotonic).
- Assert strictly increasing `pts_us`; log and drop if not (should never
  happen with reordering off).
Accept: write 120 AUs to a file; `ffprobe` reports hevc Main yuv420p; the
receiver's `h265parse` accepts it; first AU contains VPS/SPS/PPS.

### S5 — Session, menu bar app, preferences
Owns: `SenderSession.swift`, `App.swift`, `MenuView.swift`, `ConnectView.swift`,
`Preferences.swift`.
- The sender is a menu bar app (SwiftUI `MenuBarExtra`, `.window` style,
  `NSApp` activation policy `.accessory` so it has no Dock icon even when run
  unbundled). Minimum macOS 14. Menu bar icon reflects state (disconnected /
  connecting / streaming / error). The panel shows:
  - status line: Disconnected, Connecting to host:port (attempt n),
    Connected to "<name>" W×H @ Hz, Streaming, Paused, Reconnecting in n s,
    last error;
  - link: interface name and type (Wi-Fi / Ethernet / Thunderbolt bridge /
    other, from `NWPath`), RTT (min of recent PINGs, §6.8);
  - **effective bitrate**: FRAME bytes actually written to the socket
    (header + payload) over a 1 s sliding window, shown in Mbit/s next to
    the target bitrate, plus sent fps and frames skipped for backpressure;
  - last speed test: Mbit/s, when, over which interface;
  - buttons: Connect… (host + port fields, recent receivers list, default port
    7878), Disconnect, Run Speed Test (enabled when the receiver advertises
    caps bit 2), Quit.
  - settings: bitrate mode Auto (from speed test) / fixed (20, 40, 80, 150
    Mbit/s or custom), auto-reconnect on/off, launch at login
    (`SMAppService.mainApp`, only works from the .app bundle).
- Connect accepts an IPv4/IPv6 literal or a hostname plus port. Persist the
  receiver (host, port, name from HELLO) in the recent list on a successful
  HELLO. Multiple receivers can be remembered; one connection at a time.
- Disconnect is user intent: send GOODBYE NORMAL, release the display, and do
  not reconnect. Network loss / liveness timeout / GOODBYE from the peer
  reconnect with backoff 1,2,4,…,10 s when auto-reconnect is on.
- Bitrate Auto: after HELLO, run a 500 ms speed test (if caps bit 2) before
  STREAM_START and set the encoder's average bitrate to 60 % of the measured
  throughput, clamped to [10, 200] Mbit/s and to `max_bitrate_kbps` if set.
  Re-running the speed test from the menu updates it live.
- `SenderSession`: §7.2 state machine. On HELLO: validate version/codecs,
  create display (S2), start capture (S3) and encoder (S4), send
  STREAM_START (`stream_id` from 1, hidpi flag), force a keyframe. On
  KEYFRAME_REQUEST: `forceKeyframe` on the next encode. On mode change
  callback: STREAM_STOP, reconfigure capture + encoder, STREAM_START, keyframe.
  On disconnect/GOODBYE/liveness timeout: stop capture, invalidate encoder,
  release display, reconnect with backoff 1,2,4,…,10 s until SIGINT.
- Backpressure: skip captures while `connection.pendingFrames > 2`.
- Quit / SIGINT / SIGTERM: send GOODBYE NORMAL, release display, exit 0.
- Speed test (S7) pauses capture as backpressure does; no STREAM_STOP.
Accept: end-to-end against the Rust receiver with `--sink autovideosink` on a
Linux box: display appears in Settings, dragging a window onto it shows on the
receiver; unplugging the cable returns windows to the laptop within 2 s;
replugging reconnects without restarting either program.

### S6 — Packaging
Owns: `sender/scripts/`, `README.md` (sender section).
- `make-app-bundle.sh`: wraps the release binary in `ImacDisplaySender.app`
  with an `Info.plist` (`LSUIElement = true` so no Dock icon,
  `NSScreenCaptureUsageDescription`), ad-hoc codesigned, so Screen Recording
  permission attaches to the app. Install by copying to `/Applications`.
- Launch at login via the menu toggle (S5).
- Optional `--host/--port` launch arguments auto-connect on start, for
  scripted installs across many Macs.
Accept: fresh Mac follows README: grant Screen Recording once, open the app,
Connect… 10.99.0.2:7878, display appears; Disconnect removes it.

### S7 — Link stats and speed test
Owns: `IDPCore/{SpeedTest,BitrateMeter,LinkInfo}.swift`, PING handling in
`Connection.swift`.
- PING every 1 s while connected; RTT = min over the last 5 PONGs. Answer
  peer PINGs immediately.
- `SpeedTest`: PROTOCOL §6.9 sender procedure with 1 MiB DATA chunks, at most
  4 outstanding; result `{mbps, bytes, elapsed, interface, date}`; timeout
  `duration + 5 s`. The session gates capture while a test runs.
- `BitrateMeter`: ring of (timestamp, bytes) samples; `bitsPerSecond` and
  `framesPerSecond` over the last 1 s; thread-safe; unit-tested with an
  injected clock.
- `LinkInfo`: from `NWConnection.currentPath`: interface name, type
  (`bridge*` on a Thunderbolt port → "Thunderbolt bridge", `.wifi`,
  `.wiredEthernet`), local and remote address.
Accept: `swift test` covers meter math and the speed-test exchange against a
fake receiver; on a real link the menu shows a plausible Mbit/s for Wi-Fi
(~100–800), gigabit Ethernet (~900) and Thunderbolt bridge (several Gbit/s).

### R6 — Receiver speed test, PING, STATS, idle screen
Owns: `receiver/src/speedtest.rs`, PING/PONG and STATS in `session.rs`.
- Answer PING with PONG; implement §6.9 receiver rules (count filler without
  buffering it, abort stale test, timeout, restart the STREAM_START timer).
- STATS once per second while streaming (caps bit 1) from pipeline counters.
- Advertise caps bit 2 (and bit 1 when stats are wired up).
- Idle screen (stretch): while LISTENING, show the receiver's name and
  `ip:port` for each non-loopback interface on the panel so users at any Mac
  know what to type in Connect….
Accept: integration test runs a speed test over loopback and gets a
SPEED_TEST_RESULT with `bytes_received` equal to the bytes sent.

---

## 5. Milestones (suggested order)

| # | milestone                                              | tasks                 |
|---|--------------------------------------------------------|-----------------------|
| M1 | Protocol frozen, fixtures round-trip in both languages | R0, S0                |
| M2 | Receiver speaks IDP end-to-end with test_sender        | R1, R2, R4, R6        |
| M3 | Receiver on real iMac: VA-API + kmssink at native 5K   | R3, R5                |
| M4 | Mac sender pieces proven in isolation                  | S1, S2, S3, S4, S7    |
| M5 | Mac → iMac extended desktop over Thunderbolt bridge    | S5                    |
| M6 | Installable on any Mac / any Ubuntu iMac               | S6, README            |

R0/S0 first and in lockstep: everything else depends on the fixtures.
R-tasks and S-tasks are otherwise independent and can run in parallel; the
Rust `test_sender` lets receiver work proceed with no Mac, and a Linux box
running the receiver with `--sink autovideosink` lets sender work proceed
with no iMac.

---

## 6. Test matrix

| scenario                                   | where            | how                                      |
|--------------------------------------------|------------------|------------------------------------------|
| fixture round-trip                         | CI, both         | `cargo test`, `swift test`               |
| receiver ⇄ test_sender, software decode    | CI (Linux)       | `receiver/tests/integration.rs`          |
| BUSY on second connection                  | CI               | integration test                         |
| liveness timeout both directions           | CI               | integration test (pause 3 s)             |
| resolution change mid-session              | CI               | integration test                         |
| VA-API zero-copy to kmssink                | iMac             | `GST_DEBUG=va*:4`, caps show DMABuf      |
| 5K60 CPU/GPU load                          | iMac             | `top`, `radeontop`                       |
| Annex-B output validity                    | Mac              | dump AUs, `ffprobe`, `hevc_analyzer`     |
| end-to-end latency                         | Mac + iMac       | photograph both screens showing a ms timer; target ≤ 50 ms |
| unplug / replug                            | Mac + iMac       | manual; windows back ≤ 2 s, auto-reconnect |
| Ethernet / Wi-Fi instead of Thunderbolt    | Mac + iMac       | same binaries, different host in Connect…; run Speed Test on each |
| speed test exchange                        | CI, both         | integration test (Rust), fake receiver (Swift) |

---

## 7. Hardware/OS notes the implementer needs

- **Receiver GPU**: 2017–2020 iMacs have Radeon Pro 5xx/5xxx; HEVC Main decode
  via `mesa-va-drivers` (radeonsi). `vainfo` must list `VAProfileHEVCMain :
  VAEntrypointVLD`. The receiver's service user needs `/dev/dri/renderD128`
  (group `render`) and, for `kmssink`, `/dev/dri/card0` (group `video`) with no
  other DRM master on that seat.
- **Thunderbolt link**: a real Thunderbolt 3 cable; Linux `thunderbolt-net`
  loads automatically when the peer is a Mac; authorize with `boltctl` if the
  security level requires it. Addressing is manual (see `docs/network-setup.md`).
- **macOS**: `CGVirtualDisplay` is private API (macOS 14+ shape; verified on
  26.x by third-party tools). ScreenCaptureKit needs Screen Recording
  permission. Not App Store distributable.
- **Known sharp edges**: changing a virtual display's mode programmatically
  can make it un-removable until logout — only the user changes modes. macOS
  restores the last mode per vendor/product/serial; keep those stable.

---

## 8. Out of scope for v1 (tracked for v2)

- Encryption/authentication (PROTOCOL §10 reserves it).
- Multiple simultaneous senders with switching (receiver currently BUSY).
  Design hook: STREAM_STOP already expresses "paused but connected".
- Keyboard/mouse forwarding (software KVM).
- Windows and Linux senders (IddCx / PipeWire). The protocol is OS-agnostic.
- UDP/QUIC transport for Wi-Fi.
- Lossless changed-rectangle mode for sharper text.
- Continuous adaptive bitrate from STATS (v1 sets bitrate from the speed test).
- Bonjour discovery (`_idp._tcp`) so the Connect… list fills itself.
