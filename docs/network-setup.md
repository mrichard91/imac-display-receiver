# Network setup

The receiver listens on every interface (port 7878 by default) and the sender
connects to whatever address you type in **Connect…**, so any IP link works.
Pick the fastest one you have:

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

A direct Thunderbolt 3 cable between the Mac and the iMac gives a private
point-to-point network with no router and no other hosts on it. Use a /30 so
both ends are fixed:

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

v1 has no authentication or encryption (PROTOCOL.md §10): anyone who can reach
the port can put video on the panel. On a Thunderbolt or direct Ethernet link
nobody else can reach it. On a shared LAN, restrict the port to your Macs:

```sh
sudo ufw allow from 10.99.0.0/30 to any port 7878 proto tcp
sudo ufw allow from 192.168.1.20 to any port 7878 proto tcp    # a specific Mac
```
