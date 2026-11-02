# Network setup

The receiver listens on every interface, IPv4 and IPv6 (port 7878 by
default), and the sender connects to whatever address you type in
**Connect…**, so any IP link works. Because v1 has no authentication or
encryption, read [Firewall](#firewall) before connecting the iMac to anything
but the cable to your Mac. Pick the fastest link you have:

| link                         | typical throughput   | comfortable bitrate at 5K60 |
|------------------------------|----------------------|-----------------------------|
| Thunderbolt 3 bridge         | several Gbit/s       | 80–200 Mbit/s               |
| Gigabit Ethernet             | ~940 Mbit/s          | 80–150 Mbit/s               |
| Wi-Fi 5/6, same room         | 100–800 Mbit/s, variable | 20–60 Mbit/s            |

Whatever the link, run **Run Speed Test** from the sender's menu once it is
connected (or `test_sender --speed-test 1000` from a Linux box). It measures
real sender → receiver throughput over the same TCP connection the video uses
(PROTOCOL.md §6.9). With bitrate set to **Auto**, the sender uses 60 % of the
measured rate; on Wi-Fi, re-run it after moving the Mac.

## Thunderbolt bridge (recommended)

A direct Thunderbolt 3 cable between the Mac and the iMac gives a
point-to-point network with no router and no other hosts on it (the receiver
still listens on the iMac's other interfaces; see [Firewall](#firewall)).
Use a /30 so both ends are fixed:

| side  | interface         | address        |
|-------|-------------------|----------------|
| Mac   | Thunderbolt Bridge| `10.99.0.1/30` |
| iMac  | `thunderbolt0`    | `10.99.0.2/30` |

### Ubuntu (receiver)

The `thunderbolt-net` module loads automatically when the peer is a Mac. If the
port's security level requires it, authorize the Mac first:

```sh
boltctl list                                  # find the Mac's UUID
sudo boltctl enroll --policy auto <uuid>      # remembered across reboots
ip link show thunderbolt0                     # should exist now
```

Give it a static address with NetworkManager (persists across reboots and
replugs):

```sh
sudo nmcli connection add type ethernet ifname thunderbolt0 con-name thunderbolt-mac \
  ipv4.method manual ipv4.addresses 10.99.0.2/30 ipv6.method link-local
sudo nmcli connection up thunderbolt-mac
```

(For a quick test without NetworkManager:
`sudo ip addr add 10.99.0.2/30 dev thunderbolt0 && sudo ip link set thunderbolt0 up`.)

### macOS (sender)

System Settings → Network → **Thunderbolt Bridge** → Details… → TCP/IP:

- Configure IPv4: **Manually**
- IP Address: `10.99.0.1`
- Subnet Mask: `255.255.255.252`
- Router: leave empty (so the Mac keeps using Wi-Fi/Ethernet for the internet)

Then in the sender: **Connect…** → host `10.99.0.2`, port `7878`.

Check from the Mac: `ping 10.99.0.2` and `nc -vz 10.99.0.2 7878`.

## Ethernet

Either a direct cable between the two machines or both on the same switch.

- **Direct cable:** same as the Thunderbolt bridge with another subnet, e.g.
  iMac `10.99.1.2/30` on its Ethernet interface (`nmcli ... ifname enp3s0`),
  Mac `10.99.1.1/30` on its Ethernet adapter with no router.
- **Through a switch/router:** let DHCP assign addresses but give the iMac a
  DHCP reservation (or a static address) so the address you type in the
  sender's Connect… list stays valid.

## Wi-Fi

The Mac on Wi-Fi and the iMac on Ethernet (or Wi-Fi) in the same LAN works
with no extra setup; connect to the iMac's LAN address. Expect variable
throughput and latency spikes: run the speed test, prefer the 5/6 GHz band,
and use a fixed bitrate well below the measured rate if the picture stutters.

## Finding the iMac's address

On the iMac: `ip -brief addr` (look for `thunderbolt0`, `enp*`, `wl*`). The
receiver logs the address it is listening on at startup:
`journalctl -u imac-display-rx | grep listening`.

## Firewall

> [!WARNING]
> Never expose the receiver to an untrusted network or the internet. Do not
> port-forward 7878, and check that your router does not pass inbound IPv6 to
> the iMac.

v1 has no authentication or encryption
([`PROTOCOL.md` §10](../PROTOCOL.md#10-security)). In practice that means:

- the Mac's screen crosses the link in plaintext, so anyone who can capture
  the traffic can watch it;
- anyone who can reach the port can put arbitrary video on the panel, feed
  attacker-chosen HEVC to the GStreamer parser and decoder, or hold the one
  sender slot so your Mac gets `BUSY`;
- every client that connects while no sender is connected receives HELLO:
  the receiver's name (the hostname unless `RECEIVER_NAME` is set),
  `display_id` and panel geometry.

The firewall is the only access control. The receiver binds `[::]:7878`
dual-stack and accepts on **every** interface, IPv4 and IPv6, including Wi-Fi
and global IPv6 addresses, even if you only meant to use the Thunderbolt
bridge; there is no option to bind one address or interface. A cable is
private only if the iMac has no other network up, and Ubuntu ships with `ufw`
disabled. So on any iMac that is also on a LAN or Wi-Fi, deny by default and
allow only your Macs:

```sh
sudo ufw default deny incoming
sudo ufw allow OpenSSH                                          # if you use ssh
sudo ufw allow from 10.99.0.0/30 to any port 7878 proto tcp     # Thunderbolt bridge
sudo ufw allow from 192.168.1.20 to any port 7878 proto tcp     # a specific Mac
sudo ufw enable
sudo ufw status verbose
```

A Mac that connects over IPv6 needs its own `ufw allow from <ipv6-address>`
rule. Prefer one address per Mac to a whole subnet.

The same restriction can be put on the service itself, which also works when
`ufw` is off and covers IPv4 and IPv6 (`sudo systemctl edit imac-display-rx`):

```ini
[Service]
IPAddressDeny=any
IPAddressAllow=localhost 10.99.0.0/30 192.168.1.20
```

Check from a machine that should *not* get in: `nc -vz <imac-address> 7878`
must fail. Watch for peers you do not recognise with
`journalctl -u imac-display-rx | grep connected` (the receiver logs every
peer address).

Also:

- Use the receiver only on networks you control; on shared or public Wi-Fi,
  others can watch the stream even if the firewall keeps them off the port.
- On the Mac, connect by IP address on a link you control rather than by
  hostname, and turn off *Reconnect automatically* on networks you do not
  control: the sender streams to whatever answers at that address.
- Set `IDLE_SCREEN=false` if people you do not trust can see the panel; the
  idle screen shows the receiver's name, addresses (all except loopback and
  IPv6 link-local) and port.
- Only enrol the Mac with `boltctl` if `boltctl list` shows it needs
  authorization; `--policy auto` trusts that device on every future plug-in.

See [`SECURITY.md`](../SECURITY.md) for the full list of known limitations and
how to report a vulnerability.
