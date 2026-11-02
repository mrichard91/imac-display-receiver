# Security policy

This repository is the Linux receiver for the iMac Display Protocol (IDP). The
macOS sender lives in `imac-display-sender` and has its own `SECURITY.md`.

> [!WARNING]
> IDP v1 has **no authentication and no encryption**, by design. Run it only
> on a cable or a trusted LAN, firewall the port, and never expose it to an
> untrusted network or the internet. See
> [`PROTOCOL.md` §10](PROTOCOL.md#10-security).

## Scope

In scope:

- The receiver binary `imac-display-rx` (`src/`), including its handling of
  every IDP message, its resource limits and its GStreamer pipeline setup.
- The protocol specification [`PROTOCOL.md`](PROTOCOL.md) and the fixtures in
  `protocol/fixtures/`, where a flaw in the specification itself affects every
  implementation.
- The packaging and setup guidance: `systemd/imac-display-rx.service`,
  [`README.md`](README.md) and [`docs/network-setup.md`](docs/network-setup.md).

Out of scope (report upstream): bugs in GStreamer, `h265parse`, the VA-API
drivers (Mesa), `libav`/FFmpeg, the kernel or other third-party components,
unless the receiver uses them in a way that creates the problem.

## Supported versions

| version                              | supported |
|--------------------------------------|-----------|
| `main` branch, protocol v1           | yes       |
| older commits, forks                 | no        |

There are no release branches; fixes land on `main`.

## Known limitations (not vulnerabilities)

These follow from v1 having no authentication or encryption and are documented
in [`PROTOCOL.md` §10](PROTOCOL.md#10-security). Reports that only restate them
will be closed, but ideas for fixing them in v2 are welcome as ordinary issues
(see [`PLAN.md` §9](PLAN.md#9-public-release-and-next-steps)).

| limitation | consequence |
|------------|-------------|
| No encryption | The sender's screen crosses the network as plaintext HEVC; anyone on the path can record and decode it. |
| No authentication | Anyone who can reach the TCP port (default 7878) can show arbitrary video on the panel. |
| Receiver identity is not proven | A sender cannot tell a genuine receiver from any host that answers with a valid HELLO. `display_id` and the name are public values, sent to every client that connects while no sender is connected. |
| Attacker-controlled bitstream | Anyone who can connect feeds HEVC to `h265parse` and the decoder (VA-API or `avdec_h265`), which are C code outside this project. |
| One sender slot | Any client that connects first and keeps sending messages holds the slot; everyone else gets `GOODBYE BUSY`. |
| Large messages | A peer may declare messages of up to 64 MiB (PROTOCOL §4); a FRAME payload is allocated at its declared length. |
| Disclosure in HELLO | Every client that gets the free slot receives the receiver's name (the hostname by default), `display_id`, panel geometry and bitrate hint before it sends anything. |
| Listens everywhere | The receiver binds `[::]` dual-stack (IPv4 and IPv6) on all interfaces and has no bind-address option. The idle screen shows the receiver's name, addresses (all except loopback and IPv6 link-local) and port on the panel. |
| Per-connection cost | Each accepted connection gets a thread (no cap; a thread-spawn failure stops the receiver, and systemd restarts it); each STREAM_START rebuilds the pipeline; PINGs are answered without a rate limit (PLAN R11). |

Mitigations available today: firewall by source address (IPv4 and IPv6) with
`ufw` or a systemd `IPAddressAllow=` drop-in, use a dedicated cable, set
`RECEIVER_NAME` and `IDLE_SCREEN=false`, and keep GStreamer, Mesa, `libav` and
the kernel patched. See [`README.md`](README.md#5-firewall) and
[`docs/network-setup.md`](docs/network-setup.md#firewall).

## What counts as a vulnerability

Report privately anything that goes beyond the limitations above, for example:

- Memory unsafety, a panic or a crash in the receiver caused by any byte
  sequence a peer can send.
- A way to exceed the documented resource limits: memory beyond what the
  64 MiB message cap and the `appsrc` queue imply, threads or file
  descriptors beyond one short-lived thread per open connection, or a
  connection that survives the 2 s liveness timeout without sending messages.
- A way to make the receiver read, write or disclose local files or data other
  than what HELLO is specified to carry.
- Code execution, or privilege beyond the service user, starting from the
  network, including through the way the receiver drives GStreamer.
- Packaging or documentation that leaves a system more exposed than the docs
  say (for example a firewall example that does not do what it claims).
- A flaw in `PROTOCOL.md` that would defeat a security property an
  implementation is required to have.

Robustness bugs that a peer cannot trigger (for example a bad local
configuration) are ordinary issues.

## Reporting a vulnerability

Do **not** open a public issue, pull request or discussion.

Use GitHub private vulnerability reporting: open this repository's
**Security** tab and choose **Report a vulnerability**. Include the commit you
tested, the receiver's configuration (sink, decoder, Ubuntu and GStreamer
versions), the steps or bytes that trigger the problem, and the impact you
observed.

If the **Report a vulnerability** button is missing, open a public issue that
asks for a private contact, without any details.

This is a personal project maintained in spare time. You should get an
acknowledgement within about two weeks; a fix, or a documented limitation if
the problem is inherent to v1, follows as time allows. Please allow 90 days, or
until a fix is published, before disclosing publicly. Credit is given in the
fix's commit and advisory unless you ask otherwise.

If the problem is in the protocol and also affects the sender, one report in
either repository is enough.
