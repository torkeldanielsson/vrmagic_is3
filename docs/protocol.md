# Control protocol

The subset of the camera's control protocol that `is3` uses, as measured on
cameras with firmware `A1030204`. All multibyte fields are big-endian.

## Transport and framing

Control uses unicast UDP port **50001**. The camera's IPv6 link-local address
is its modified EUI-64 address: insert `ff:fe` in the MAC and flip the U/L bit.
The host must supply the scope ID of its camera-facing Ethernet interface.

```
Request: u32 transaction_id | u16 opcode | u16 length | payload
Reply:   u32 transaction_id | u16 status | u16 length | payload
```

Use a fresh transaction ID for each command, and reuse it only when retrying
that same command. A reused ID returns a stale reply that makes valid
commands look unsupported. Status zero is success. Only accept replies with the matching
ID and exact declared length. Pace commands; this is a small embedded target. Issue commands one at a
time: image-pipeline commands share an argument pool, so a watchdog feed must
not be interleaved into the middle of a multi-command operation.

| Opcode | Payload / result |
|---|---|
| `c905` | Payload `80 0c 00 00`; returns the firmware identifier, `a1 03 02 04` on the cameras measured |
| `c220` | Empty payload; result six-byte seconds then four-byte nanoseconds |
| `8101` | Get state; first response byte is the state |
| `8100` | `28 00`: apply/start, expect state `31`; `40 00`: suspend, expect `41` |
| `c210` | `07 d0 00 00`: delayed software reboot; send once and verify clock rollback |
| `c501/c503/c505` | Logical read, respectively 8/16/32-bit elements |
| `c500/c502/c504` | Logical write, respectively 8/16/32-bit elements |
| `c403` | Physical 16-bit read, used for ISP command-pending register `0040` |
| `c901/c900` | Read/write the live network video configuration |

Register read payloads are `u16 address | u16(count - 1)`. Writes append the
element bytes. Software reboot restores volatile defaults, but it does not
necessarily restart the external watchdog deadline.

## Feeding the watchdog through GPIO4

GPIO mask `0x00000010` selects pin 4. See [investigation.md](investigation.md)
for why this pin and no other.

| Operation | Opcode | Payload |
|---|---|---|
| Host claims pin | `8400` | `00 00 00 10 00 01` |
| Output direction | `8400` | `00 00 00 10 01 00` |
| Enable | `8400` | `00 00 00 10 02 01` |
| Drive high | `8402` | `00 00 00 10 00 00 00 10` |
| Drive low | `8402` | `00 00 00 10 00 00 00 00` |

Alternate high and low every ten seconds; the watchdog expires about 101 s
after the last edge. Release in the order enable=0, direction=1, ownership=0.
**Do not generalise this to other pins:** GPIO2 driven low hard-resets the
board, and GPIO6 reconfigures the watchdog.

## Video configuration

Read the 68-byte runtime network descriptor with `c901`, payload
`a0 00 00 43`. Write with `c900`, the same prefix followed by all 68 bytes.
Preserve fields that are not being changed.

| Descriptor offset | Meaning used by this tool |
|---|---|
| `0..4` | Flags: transport bits 17:16 = 2 for IPv4; bit 0 selects explicit destination MAC |
| `4..10` | Destination Ethernet MAC |
| `28..44` | Source address slot; IPv4 in first four bytes, remaining bytes zero |
| `44..60` | Destination address slot, same representation |
| `60..62` | Source UDP port, preserved |
| `62..64` | Destination UDP port |
| `64` | Camera emits payload type 26; repaired H.264 relay changes it to 96 |

The source IPv4 value comes from ISP logical `8c28`; it need not be on the
receiver's IP subnet because the camera sends to an explicitly supplied MAC.
Receiver routing/firewall rules must still accept that source.
`8c88` is a startup copy, not the live descriptor to edit.

| Logical address | Configuration |
|---|---|
| `cae4` | Width and height, two u16 values |
| `caf4` | Target bitrate, u16 in units of 8000 bit/s |
| `caea` | Codec: 3 = MJPEG, 4 = H.264 |
| `caf8` | H.264 flags: bit 6 selects RFC 6184 packetization; bit 5 selects 10-bit; preserve the other bits |

Suspend an active stream, write and read back the network descriptor, apply
the codec/profile, then start. Returning from H.264 to MJPEG needs a reboot
first (`--reset`).

## Repairing H.264

The camera's H.264 packets declare IP/UDP lengths two bytes short; see
[video.md](video.md). For the measured IPv4/RFC 6184 profile the relay:

1. Capture complete Ethernet frames from the selected camera and UDP port.
2. Require matching IPv4 and UDP declared ends, reject IP fragments, and retain
   **two additional Ethernet payload bytes** after that end.
3. Validate a simple RTP v2 header and camera payload type 26; change it to 96.
4. Group packets by SSRC and timestamp. Require the initial SPS, sequential
   packet numbers, and complete FU-A fragments; discard damaged pictures.
5. Set the marker only on the last packet of a complete picture and transmit
   through a normal UDP socket, which supplies correct lengths/checksums.

Standard references: [RTP](https://www.rfc-editor.org/rfc/rfc3550),
[RTP/JPEG](https://www.rfc-editor.org/rfc/rfc2435),
[RTP/H.264](https://www.rfc-editor.org/rfc/rfc6184).
