# vrmagic_is3

A standalone controller for VRmagic IS3 cameras.

The IS3 is a 1080p PoE camera that was sold as part of a surveillance system
with a separate gateway box. Without that gateway the camera is useless: it
offers no web page, no RTSP, no GigE Vision, and it reboots itself every
203 seconds. This repository contains one Rust binary, `is3`, that talks the
camera's native control protocol, keeps its on-board watchdog fed so it stays
up, and delivers standard RTP video to any receiver you like.

No vendor software or firmware flashing is involved. The
protocol, the watchdog and the video quirks were all reverse engineered; the
story is in [docs/investigation.md](docs/investigation.md).

## What you get

- **Status.** Firmware, streaming state and the camera's clock.
- **Keepalive.** Feeds the external watchdog so an idle camera stops rebooting.
- **MJPEG straight from the camera.** Standard RTP/JPEG (RFC 2435), sent by
  the camera directly to your receiver. Nothing else touches the video.
- **H.264 through a small relay.** The camera's H.264 packets are malformed
  on the wire. The relay repairs them without re-encoding and forwards
  standard RTP/H.264 (RFC 6184), 8-bit or 10-bit.

## Requirements

- Linux on the machine that runs `is3`, with an Ethernet interface on the
  same LAN as the camera. Only this machine needs the tool.
- A PoE switch or injector for the camera. The camera links at 100 Mbit.
- A stable Rust toolchain from [rustup.rs](https://rustup.rs) to build.

The H.264 relay captures raw Ethernet frames and therefore needs
`CAP_NET_RAW` (run with `sudo`, or `setcap cap_net_raw+ep` on the binary).
Status, keepalive and direct MJPEG need no privileges.

## Install

```sh
cargo install --locked --git https://github.com/torkeldanielsson/vrmagic_is3
```

This puts `is3` in `~/.cargo/bin`. Add `--force` to upgrade later.

## Quick start

Every command takes the camera's MAC address and the name of the Ethernet
interface it is reachable on. The camera is addressed over IPv6 link-local,
derived from the MAC, so no DHCP server or IP configuration is needed. The
MAC is printed on the camera label, or visible in your switch.

All MAC and IP addresses in this document are examples.

```sh
# Is it alive?
is3 status --camera 00:21:21:01:02:03 --interface eth0
```

### MJPEG, no relay

The simplest and most robust path. The receiver must be on the same Ethernet
segment as the camera, and you must give both its IPv4 address and its MAC
(`ip -4 addr` and `ip link` on Linux):

```sh
is3 stream --camera 00:21:21:01:02:03 --interface eth0 \
  --codec mjpeg --direct \
  --to 192.0.2.20 --destination-mac 02:00:00:00:00:20 \
  --port 5004
```

The camera now sends RTP/JPEG to `192.0.2.20:5004`. The controlling machine
only exchanges control and watchdog traffic with the camera. Leave the
command running; stopping it releases the watchdog and the camera will
eventually reboot again.

Add `--reset` if the camera was previously streaming H.264: that switch
needs a camera reboot first.

### H.264, through the relay

```sh
sudo "$(command -v is3)" stream \
  --camera 00:21:21:01:02:03 --interface eth0 \
  --codec h264 --bit-depth 8 \
  --to 192.0.2.20 --port 5004
```

The camera streams to the controlling machine, the relay fixes the packets
and forwards RTP/H.264 with payload type 96 to the receiver. The receiver can
be anywhere IP routing allows, including `127.0.0.1` (the default for
`--to`). Buffering one complete picture before forwarding adds about one
frame of latency.

`--bit-depth 10` selects High 10 Intra. The default profile is 1920×1080 at
a requested 12 Mbit/s; `--width`, `--height` and `--bitrate-mbps` change it.
Before relying on 10-bit or on colour accuracy, read
[docs/video.md](docs/video.md): the camera omits some metadata and many
receivers decode to 8-bit surfaces regardless.

### Receiving the stream

Any RTP receiver works. `--sdp camera.sdp` writes an SDP file describing the
stream; copy it to the receiving machine and play it:

```sh
ffplay -protocol_whitelist file,udp,rtp camera.sdp
```

Or with GStreamer, for example:

```sh
# MJPEG
gst-launch-1.0 udpsrc port=5004 \
  caps="application/x-rtp,media=video,encoding-name=JPEG,payload=26" \
  ! rtpjpegdepay ! jpegdec ! autovideosink

# H.264 from the relay
gst-launch-1.0 udpsrc port=5004 \
  caps="application/x-rtp,media=video,encoding-name=H264,payload=96" \
  ! rtph264depay ! avdec_h264 ! autovideosink
```

Remember to open the port in the receiver's firewall. This is plain RTP over
UDP, not RTSP.

### Keep an idle camera alive

```sh
is3 keepalive --camera 00:21:21:01:02:03 --interface eth0
```

Useful while you experiment with your own client. Without a feeder the
camera reboots about 101 seconds after the last watchdog edge.

### Several cameras

Run one `is3` process per camera, each with its own receiver port. A process
holds a local lock for its camera; it cannot stop another machine on the LAN
from talking to the same camera, so coordinate that yourself.

`keepalive` and `stream` accept `--seconds N` for bounded runs. Both stop
cleanly on Ctrl+C or SIGTERM, and `stream` exits with an error if the camera
reboots or the video stops, so a supervisor can restart it.

## Scope

The tool changes volatile settings only; it never writes the camera's flash.
Of the camera's GPIO pins it drives just the watchdog feed. Two other pins
force a hard reset and reconfigure the watchdog, and are left alone. Video is
forwarded as the camera encoded it: no re-encoding, scaling or colour
conversion.

## Documentation

- [How the camera was reverse engineered](docs/investigation.md): the
  protocol, the firmware, the board, and the hunt for the 203-second reboot.
- [Control protocol](docs/protocol.md): the commands and registers the tool
  uses, for anyone writing their own client.
- [Video formats and receiver caveats](docs/video.md): what each path
  delivers on the wire, the packet defect the relay repairs, bit depth and
  colour-range caveats.

This is an independent project with no connection to VRmagic.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

The packet-repair code is covered by tests with synthetic packets, including
truncated captures, lost fragments and sequence-number wrap. Hardware
behaviour is documented in `docs/`.

## License

MIT, see [LICENSE](LICENSE).
