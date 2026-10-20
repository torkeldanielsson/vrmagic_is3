# How the IS3 was reverse engineered

Two VRmagic IS3 cameras, no gateway, no installer, no documentation beyond a
[sales brochure](https://pyramidimaging.com/uploads/Media/5419.pdf). The
brochure describes an industrial surveillance system: 1080p colour, interchangeable
lenses, 100 Mbit Ethernet with PoE, and a gateway box that provided the user
interface. Plugged into a switch, a bare camera links up, answers nothing on
any port a normal client would try, and reboots every 203 seconds.

This is the story of making it work, written so that the findings can be
checked and extended.

## First contact

The camera does not take an IPv4 address by DHCP, but like any modern stack
it has an IPv6 link-local address derived from its MAC. Probing that address
found a UDP service on port 50001 that answered binary requests.

The replies only made sense once every request carried a fresh transaction
identifier: reuse an identifier and the camera returns a stale reply, which
makes perfectly valid commands look unsupported. With that sorted, the
protocol turned out to be ON Semiconductor's AP020x host-command protocol
(ACCP) with big-endian headers. From there it was possible to read firmware
identity, drive the state machine, read and write configuration memory, talk
to the image sensor, and start JPEG and H.264 streams. The subset the tool
uses is written up in [protocol.md](protocol.md).

## What the firmware looks like

The camera is built around an AP02-family imaging processor with integrated
Ethernet, plus an external PHY. Inside the processor are two ARC600-class
cores with separate firmware: one runs the image pipeline, the other runs the
network stack (ThreadX and NetX Duo, by their version strings).

Both ROMs, the external flash's initialisation records and six factory patches
were recovered and analysed in Ghidra. Small temporary routines were loaded
into RAM on both cores to verify calling conventions, command dispatch, patch
loading, GPIO behaviour and the Ethernet receive path. That was enough to
instrument the firmware, though not for a complete schematic or a
replacement firmware. The tool needs neither: ordinary host commands do
everything.

## The 203-second reboot

Both cameras rebooted every 203 seconds, like clockwork, idle or streaming.
Everything plausible was suspected in turn: PoE negotiation, the switch, a
CPU watchdog, a missing network service, a proprietary heartbeat from the
gateway.

Each was ruled out in turn:

- The switch kept supplying PoE through every reboot, as seen from the
  switch's own 1 Hz power telemetry. The reset is generated on the board.
- The link went up and down with crystal-like regularity on the switch's
  clock: 200.3 s up, 2.7 s down, for 130 consecutive cycles.
- A software reboot or a PHY reset did not move the next deadline. Only a
  power cycle did. Whatever kept time was outside the processor's reset
  domain.
- Parking both CPU cores with interrupts masked did not stop it either.
- Establishing a full gPTP peer session, feeding the sensor bus, SPI flash
  reads, PHY LED and interrupt changes: none postponed it.
- The two cameras behaved identically and their flash contents matched
  byte for byte apart from per-unit network settings.

Opening the spare camera showed the answer sitting next to the PHY: an 8-pin
part marked `AADE`, which is a **MAX6371** external watchdog, beside a
four-channel level translator. A MAX6371 has a fixed startup delay, a
selectable timeout, and a watchdog input (WDI) that must see an edge before
the timeout expires. Nothing in the firmware ever produced one.

The watchdog's pins can only reach the processor through that translator, so
the final experiment was to drive the processor's seven host-controllable GPIO
pins and watch what happened:

| Action | Result |
|---|---|
| Toggle GPIO4 continuously | Camera stays up indefinitely |
| Stop toggling GPIO4 | Reboot 101 s after the last edge |
| Hold GPIO4 at a fixed level | Normal reboot; a level is not an edge |
| Toggle GPIO6 | Camera stays up; reboot 203 s after the last change |
| Hold GPIO6 low | No reboots while held; reboot 203 s after release |
| Pulse GPIO2 low for 200 ms | Immediate hard reset |
| Toggle GPIO0, 1, 3 or 5 | No effect |

Read against the datasheet: GPIO4 is WDI. GPIO6 is a SET pin (SET2: pulling
it low from the pulled-up default selects the "disabled" code, and any change
restarts startup delay plus timeout). GPIO2 reaches the reset circuit. The
measured intervals, about 102 s startup delay and about 101 s timeout, are
both 1.7× the datasheet's nominal minimum of 60 s, well inside its tolerance
band. Every hard reset changes the level on GPIO6, which is why each cycle is
the full 203 s rather than just the timeout, and why a software reboot, which
leaves the pad state alone, did not touch the schedule.

The original system presumably had the gateway feed the watchdog over this
same host-command path, so a camera that lost its gateway would reboot into a
clean state. Without the gateway, that fail-safe is all the camera ever does.

The fix is a few bytes of protocol: claim GPIO4, make it an output, and
alternate its level every ten seconds. The tool does exactly that and nothing
else with the GPIO bank. It does not use the "disable" trick on GPIO6, so if
the controller dies the watchdog still brings the camera back to a known
state.

The MAX6369–MAX6374 datasheet is on the
[Analog Devices site](https://www.analog.com/media/en/technical-documentation/data-sheets/MAX6369-MAX6374.pdf).
Electrical continuity and waveforms have not been measured; the pin roles are
inferred from behaviour that matches the datasheet exactly.

## What is on the board

| Part | How it was identified | Confidence |
|---|---|---|
| Imaging/network processor | ON Semiconductor AP02-family package; overlays work, which the manufacturer's selection guide attributes to AP0200 rather than AP0201 | Family certain, exact order code unreadable |
| Image sensor | Answers at 7-bit CCI address `0x10`; ID register `0x3000 = 0x1056`, revision `0x300e = 0x2200` | AR0230 is the candidate; not confirmed |
| Ethernet PHY | `KSZ8051MNLU` package marking; MII, 100 Mbit | Certain |
| Watchdog | `AADE` marking = MAX6371 | Certain, behaviour verified |
| Level translator | `YF04…` marking, TXS0104E package | Probable |
| PoE controller | `LGBV` marking = LT4275B | Certain, temperature grade unknown |
| Isolated flyback controller | `3748` marking = LT3748 | Certain |
| SPI flash | 128 KiB dumped; JEDEC ID `1f 42` | AT25DF011 or AT25DN011 |

Regulators, debug pads, sensor wiring and most board nets remain untraced.

## The video quirks

With the camera alive, the video had its own problems:

- **H.264 packets are two bytes short on the wire.** The Ethernet frames
  contain the full data, but the IP and UDP headers declare lengths two bytes
  too small. A normal UDP socket never sees the end of the SPS or of each
  slice fragment, and decoders reject the stream.
- **The H.264 stream advertises the JPEG payload type (26).**
- **H.264 omits colour-range metadata** although the samples are full range.

MJPEG has none of these problems and works with any RTP/JPEG receiver
directly. For H.264 the tool captures raw Ethernet frames, restores the two
bytes, rewrites the payload type and forwards complete pictures over a normal
socket. Details and measurements are in [video.md](video.md).

## Open questions

- A scope on the MAX6371's WDI, WDO and SET pins would turn the inferred pin
  roles into measured ones.
- The processor and sensor order codes are still unread.
- A camera-side fix for the H.264 length defect would remove the relay, but
  needs a firmware patch.
- Receivers that want true 10-bit output or correct colour range need to
  handle the missing metadata themselves; see [video.md](video.md).
