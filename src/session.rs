use crate::{control::Camera, media::Codec};
use anyhow::{Result, ensure};
use std::net::Ipv4Addr;

pub struct Profile {
    pub codec: Codec,
    pub bit_depth: u8,
    pub width: u16,
    pub height: u16,
    pub bitrate: f64,
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (64..=1920).contains(&self.width)
                && (40..=1080).contains(&self.height)
                && self.width.is_multiple_of(8)
                && self.height.is_multiple_of(8),
            "Dimensions must be multiples of 8 in 64×40..1920×1080"
        );
        ensure!(
            self.bitrate.is_finite() && (0.008..=75.0).contains(&self.bitrate),
            "Bitrate must be 0.008..75 Mbps"
        );
        ensure!(
            [8, 10].contains(&self.bit_depth) && (self.codec == Codec::H264 || self.bit_depth == 8),
            "Only H.264 supports 10-bit selection"
        );
        Ok(())
    }
}

pub fn configure(
    camera: &mut Camera,
    profile: &Profile,
    destination: Ipv4Addr,
    mac: [u8; 6],
    port: u16,
) -> Result<()> {
    profile.validate()?;
    let pending = camera.read(0x40, 1, 16, false)?;
    ensure!(
        u16::from_be_bytes(pending.try_into().unwrap()) & 0x8000 == 0,
        "ISP command pending; use --reset"
    );
    let codec = camera.read(0xcaea, 1, 8, true)?[0];
    ensure!([3, 4].contains(&codec), "Unknown codec state; use --reset");
    ensure!(
        profile.codec != Codec::Mjpeg || codec != 4,
        "H.264 → MJPEG requires --reset"
    );
    let state = camera.state()?;
    ensure!(
        [0x20, 0x31, 0x41].contains(&state),
        "Unexpected camera state {state:02x}"
    );
    if state == 0x31 {
        camera.transition(0x40, 0x41)?
    }
    let mut config = camera.command(0xc901, &[0xa0, 0, 0, 67], 5)?;
    ensure!(config.len() == 68, "Unexpected video configuration length");
    let flags = (u32::from_be_bytes(config[..4].try_into()?) & !0x30000) | 0x20001;
    config[..4].copy_from_slice(&flags.to_be_bytes());
    config[4..10].copy_from_slice(&mac);
    config[28..44].fill(0);
    config[28..32].copy_from_slice(&camera.read(0x8c28, 1, 32, true)?);
    config[44..60].fill(0);
    config[44..48].copy_from_slice(&destination.octets());
    config[62..64].copy_from_slice(&port.to_be_bytes());
    config[64] = 26;
    let mut payload = vec![0xa0, 0, 0, 67];
    payload.extend(&config);
    camera.command(0xc900, &payload, 1)?;
    ensure!(
        camera.command(0xc901, &[0xa0, 0, 0, 67], 5)? == config,
        "Video configuration readback differs"
    );
    camera.write(
        0xcae4,
        &[profile.width.to_be_bytes(), profile.height.to_be_bytes()].concat(),
        16,
    )?;
    camera.write(
        0xcaf4,
        &((profile.bitrate * 125.0).round() as u16).to_be_bytes(),
        16,
    )?;
    if profile.codec == Codec::H264 {
        let flags = camera.read(0xcaf8, 1, 8, true)?[0];
        camera.write(
            0xcaf8,
            &[(flags & !0x60) | 0x40 | if profile.bit_depth == 10 { 0x20 } else { 0 }],
            8,
        )?;
        camera.write(0xcaea, &[4], 8)?;
    }
    camera.transition(0x28, 0x31)
}

pub fn sdp(destination: Ipv4Addr, port: u16, codec: Codec) -> String {
    let (pt, name) = if codec == Codec::H264 {
        (96, "H264")
    } else {
        (26, "JPEG")
    };
    format!(
        "v=0\no=- 0 0 IN IP4 {destination}\ns=VRmagic IS3\nc=IN IP4 {destination}\nt=0 0\nm=video {port} RTP/AVP {pt}\na=rtpmap:{pt} {name}/90000\n{}",
        if codec == Codec::H264 {
            "a=fmtp:96 packetization-mode=1\n"
        } else {
            ""
        }
    )
}
