# Video formats and receiver caveats

What each path puts on the wire, what was measured, and what a receiver has
to decide for itself. These are four separate questions: do the packets
arrive intact, do they decode, what precision comes out, and how should the
colours be interpreted.

## The paths

| Path | On the wire | Status |
|---|---|---|
| MJPEG, direct from camera | RTP/JPEG (RFC 2435), payload type 26, IPv4 unicast | Works with standard receivers, no relay |
| MJPEG through the relay | Same, forwarded by the controller | Implemented for routed receivers; less tested than direct |
| 8-bit H.264, direct from camera | RTP/H.264 with two bytes missing per packet and payload type 26 | Fails in every standard receiver tried |
| 8-bit H.264 through the relay | RTP/H.264 (RFC 6184), payload type 96, Constrained Baseline, `yuv420p` | Works |
| 10-bit H.264 through the relay | RTP/H.264, payload type 96, High 10 Intra, `yuv420p10le` | Decodes; see precision below |

The camera also has IPv6 RTP, an Annex-B variant of its RTP framing and raw
Ethernet AVB modes. They were explored during the investigation, have their
own defects, and are not offered by the tool.

## The H.264 packet defect

Every H.264 packet the camera sends has IP and UDP length fields two bytes
smaller than the data actually present in the Ethernet frame. Any normal UDP
socket hands the application a payload missing its last two bytes. For the
SPS that means a truncated `time_scale` field and a decoder error before the
first picture; for slice fragments it means silently corrupt pictures.

The relay captures the complete Ethernet frames with a raw socket, checks
that the IPv4 and UDP declared ends agree, keeps the two extra bytes, and
rebuilds the packets:

1. Validate the RTP header and the camera's payload type 26; rewrite it to 96.
2. Group packets by SSRC and timestamp into pictures.
3. Require the initial SPS, consecutive sequence numbers and complete FU-A
   fragments; drop damaged pictures whole rather than forwarding fragments.
4. Set the marker bit on the last packet of a complete picture only.
5. Send through an ordinary UDP socket, which produces correct lengths and
   checksums.

## What was measured

Both cameras ran simultaneously for 300 seconds, one on repaired 8-bit H.264
and one on direct MJPEG, with the watchdog fed throughout. Camera clocks
advanced continuously past the old 203-second reboot interval. The H.264
relay forwarded 6,782 pictures in 286,493 packets and discarded none; a
receiver using NVIDIA NVDEC reported 6,781 successful decodes with no errors
or concealment. Shorter runs gave 791/791 successful decodes for 10-bit and
767 successes from 794 status queries for direct MJPEG, again with no errors.

FFmpeg consumed the direct MJPEG stream for 270 seconds in the same session.
It printed occasional non-monotonic DTS warnings, so timestamp behaviour
deserves its own test before using the stream for recording or
synchronisation.

FFprobe on captured bitstreams:

| Bit depth | Profile | Pixel format | Size |
|---|---|---|---|
| 8 | Constrained Baseline | `yuv420p` | 1920×1080 |
| 10 | High 10 Intra | `yuv420p10le` | 1920×1080 |

Both are 4:2:0. No other chroma format has been seen from the camera.

## Precision: accepted 10-bit is not delivered 10-bit

The 10-bit stream is genuine High 10 Intra and decoders accept it. That does
not mean your pipeline keeps the extra two bits. The NVDEC-based receiver used
for the measurements above requests an 8-bit NV12 output surface and declares
zero extra bit depth in its sequence parameters, so its "successful decode" of
10-bit input is an 8-bit result. Check your own decoder's surface format,
upload path and storage before claiming 10-bit output, and verify it with a
test that distinguishes adjacent 10-bit values rather than multiples of four.

## Colour range: a camera metadata defect

Camera-generated test overlays with known grey values
(`0, 8, 16, 32, 64, 128, 192, 235, 248, 255`) come out of the decoder at
exactly those values in JPEG and 8-bit H.264, and at exactly four times those
values in 10-bit H.264. The camera encodes full-range samples.

Its H.264 SPS, however, omits `video_signal_type_present_flag`, so a decoder
that defaults to limited range will stretch the values and clip shadows and
highlights. The relay does not rewrite the SPS. The fix belongs either in the
receiver, as a full-range override, or in a future SPS rewrite in the relay.

Colour matrix, primaries and transfer characteristics are likewise
unsignalled. Receivers typically assume BT.709; nothing establishes that this
matches the camera, and nothing in this project has calibrated colour.

## If you want to go further

| Goal | Work | How to know it is done |
|---|---|---|
| Correct colour range | Receiver override or SPS signalling, plus explicit matrix choice | Known grey and colour overlays survive decode and display without clipping |
| True 10-bit output | Negotiate and carry a higher-precision surface through the whole pipeline | Adjacent 10-bit values are distinguishable at the output |
| No relay for H.264 | Fix packet generation on the camera | A plain UDP socket receives complete SPS and slices with the right payload type |
| Other receiver platforms | Exercise each actual decoder backend | Identical saved test vectors decode and inspect correctly on each |

For most uses, direct MJPEG is the path that avoids all of this.
