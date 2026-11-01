# imac-display-receiver

Turns an iMac running Ubuntu into an extra display for a Mac. The Mac runs the
menu bar sender (`imac-display-sender` repository), which creates a virtual
display, encodes it as HEVC and streams it here over any IP link: a
Thunderbolt bridge, Ethernet or Wi-Fi. This receiver decodes the stream with
VA-API and shows it full screen on the panel with `kmssink`.

- Wire format: [`PROTOCOL.md`](PROTOCOL.md) (normative).
- Implementation plan: [`PLAN.md`](PLAN.md) (tasks R0–R6 are this repository).
- Network addressing: [`docs/network-setup.md`](docs/network-setup.md).

One sender at a time; a second Mac that connects while one is streaming gets
`GOODBYE BUSY`. v1 has no authentication or encryption (PROTOCOL.md §10), so
keep the port on a point-to-point link or a trusted LAN.

## Quick start (Ubuntu 24.04)

### 1. Install dependencies

```sh
sudo apt update
sudo apt install -y build-essential pkg-config curl \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-libav \
  mesa-va-drivers vainfo
```

Check that the GPU can decode HEVC (2017–2020 iMacs with Radeon Pro GPUs use
`radeonsi`):

```sh
vainfo | grep -i hevc            # expect: VAProfileHEVCMain : VAEntrypointVLD
gst-inspect-1.0 vah265dec kmssink | grep -E '^Factory|Long-name'
```

Ubuntu's packaged Rust is too old (the crate uses edition 2024 and needs Rust
1.88 or newer), so install it with rustup:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

### 2. Build

```sh
git clone <this repository> imac-display-receiver
cd imac-display-receiver
cargo build --release
cargo test --release            # optional: protocol fixtures + loopback tests
```

### 3. Run it once by hand

`kmssink` must be the DRM master, so nothing else may be drawing on the panel.
Switch to a text console (Ctrl+Alt+F3), log in, and run:

```sh
./target/release/imac-display-rx
```

It probes the panel (`/sys/class/drm`), then listens on all interfaces on
port 7878:

```
INFO  [display] panel eDP-1 5120x2880 @ 60.000 Hz, 597x336 mm
INFO  [gst] decoder vah265dec, sink kmssink
INFO  [net] listening on [::]:7878 as "imac"
```

While no sender is connected the panel shows the receiver's name and the
`address:port` of each interface. On the Mac, choose **Connect…** in the
sender's menu and enter one of them (for example `10.99.0.2`, port `7878`).

Useful options (each also has an environment variable, for systemd):

| flag                  | env                 | default    | meaning                                  |
|-----------------------|---------------------|------------|------------------------------------------|
| `--port`              | `PORT`              | `7878`     | TCP port, all interfaces                  |
| `--sink`              | `SINK`              | `kmssink`  | `autovideosink` to run inside a desktop   |
| `--decoder`           | `DECODER`           | `auto`     | `vah265dec`, `vaapih265dec`, `avdec_h265` |
| `--log-level`         | `LOG_LEVEL`         | `info`     | `debug` shows every keyframe request etc. |
| `--name`              | `RECEIVER_NAME`     | hostname   | name shown in the sender's menu           |
| `--max-bitrate-kbps`  | `MAX_BITRATE_KBPS`  | `0`        | bitrate hint for senders (0 = none)       |
| `--idle-screen`       | `IDLE_SCREEN`       | `true`     | show name + `address:port` while idle     |

If the panel probe gets something wrong, override it with `PANEL_WIDTH`,
`PANEL_HEIGHT`, `PANEL_REFRESH_MHZ` (60 Hz = `60000`) and `PANEL_MM`
(`597x336`).

Decoder debugging: `GST_DEBUG=va*:4 ./target/release/imac-display-rx` should
show `vah265dec` negotiating `video/x-raw(memory:DMABuf)` or `VAMemory` straight
into `kmssink` (no `videoconvert` on the hardware path).

### 4. Run as a service

```sh
sudo install -m 755 target/release/imac-display-rx /usr/local/bin/
sudo useradd --system --no-create-home --shell /usr/sbin/nologin \
  --groups video,render imac-display
sudo install -m 644 systemd/imac-display-rx.service /etc/systemd/system/
```

The panel must be free for `kmssink`: boot to the console instead of the
desktop (or stop GDM) on the iMac.

```sh
sudo systemctl set-default multi-user.target    # undo: graphical.target
sudo systemctl disable --now gdm3                # if it is running now
sudo systemctl daemon-reload
sudo systemctl enable --now imac-display-rx
journalctl -u imac-display-rx -f
ss -ltn | grep 7878                              # LISTEN on *:7878 (IPv4 + IPv6)
```

Per-machine settings go in `/etc/default/imac-display-rx`, for example:

```sh
RECEIVER_NAME=Studio iMac
MAX_BITRATE_KBPS=150000
```

### 5. Firewall

Only needed if `ufw` is enabled. Allow the sender's address (or subnet) rather
than the whole network:

```sh
sudo ufw allow from 10.99.0.0/30 to any port 7878 proto tcp   # Thunderbolt bridge
sudo ufw allow from 192.168.1.0/24 to any port 7878 proto tcp # a trusted LAN
```

## Testing without a Mac

`examples/test_sender.rs` speaks the protocol from any Linux or macOS box with
GStreamer and `x265enc`:

```sh
# Stream a 1080p test pattern for 10 s
cargo run --release --example test_sender -- --host 10.99.0.2 --width 1920 --height 1080 --seconds 10
# Measure the link (PROTOCOL.md §6.9)
cargo run --release --example test_sender -- --host 10.99.0.2 --speed-test 1000
```

To try the receiver on a desktop machine, run it in a window instead of on the
panel: `imac-display-rx --sink autovideosink --decoder avdec_h265`.

## Layout

```
src/protocol.rs    message types, encode/decode (fixtures in protocol/fixtures/)
src/net.rs         framed reader/writer, listener
src/session.rs     receiver state machine, liveness, STATS, keyframe requests
src/slot.rs        one-sender-at-a-time admission (BUSY)
src/idle.rs        idle screen: name and addresses while no sender is connected
src/speedtest.rs   receiver side of the speed test
src/pipeline.rs    GStreamer: appsrc → h265parse → decoder → queue → sink
src/panel.rs       DRM probe, EDID, display_id
examples/test_sender.rs, tests/integration.rs, systemd/, docs/
```
