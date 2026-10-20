//! Camera RTP packets as they appear in raw Ethernet frames, and picture reassembly.
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq)]
pub enum Codec {
    H264,
    Mjpeg,
}

pub fn extract(frame: &[u8], mac: [u8; 6], port: u16, codec: Codec) -> Option<Vec<u8>> {
    if frame.len() < 34 || frame[6..12] != mac {
        return None;
    }
    let mut ether = u16::from_be_bytes(frame[12..14].try_into().ok()?);
    let mut ip = 14;
    for _ in 0..2 {
        if ![0x8100, 0x88a8].contains(&ether) {
            break;
        }
        ether = u16::from_be_bytes(frame.get(ip + 2..ip + 4)?.try_into().ok()?);
        ip += 4;
    }
    let header = frame.get(ip..ip + 20)?;
    if ether != 0x0800 || header[0] >> 4 != 4 || header[9] != 17 {
        return None;
    }
    let ihl = usize::from(header[0] & 15) * 4;
    if ihl < 20 || u16::from_be_bytes([header[6], header[7]]) & 0x3fff != 0 {
        return None;
    }
    let end = ip + usize::from(u16::from_be_bytes([header[2], header[3]]));
    let udp = ip + ihl;
    let header = frame.get(udp..udp + 8)?;
    let destination = u16::from_be_bytes([header[2], header[3]]);
    let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
    if destination != port || length < 20 || udp + length != end {
        return None;
    }
    let extra = if codec == Codec::H264 { 2 } else { 0 };
    let mut rtp = frame.get(udp + 8..end + extra)?.to_vec();
    if rtp[0] != 0x80 || rtp[1] & 127 != 26 {
        return None;
    }
    if codec == Codec::H264 {
        rtp[1] = (rtp[1] & 128) | 96
    }
    Some(rtp)
}

pub struct Frames {
    codec: Codec,
    key: Option<[u8; 8]>,
    packets: Vec<Vec<u8>>,
    previous: Option<u16>,
    count: usize,
    bytes: usize,
    bad: bool,
    vcl: bool,
    fragment: bool,
    jpeg_end: bool,
    pub discarded: u64,
}

impl Frames {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            key: None,
            packets: vec![],
            previous: None,
            count: 0,
            bytes: 0,
            bad: false,
            vcl: false,
            fragment: false,
            jpeg_end: false,
            discarded: 0,
        }
    }
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        let complete = self.count > 0
            && !self.bad
            && if self.codec == Codec::Mjpeg {
                self.jpeg_end
            } else {
                self.vcl && !self.fragment
            };
        let mut result = vec![];
        if complete {
            result = std::mem::take(&mut self.packets);
            for p in &mut result {
                p[1] &= 127;
            }
            if let Some(last) = result.last_mut() {
                last[1] |= 128;
            }
        } else if self.count > 0 {
            self.discarded += 1
        }
        self.key = None;
        self.packets.clear();
        self.previous = None;
        self.count = 0;
        self.bytes = 0;
        self.bad = false;
        self.vcl = false;
        self.fragment = false;
        self.jpeg_end = false;
        result
    }
    pub fn push(&mut self, packet: Vec<u8>) -> Vec<Vec<u8>> {
        if packet.len() < 13 || packet[0] != 0x80 {
            return vec![];
        }
        let key: [u8; 8] = packet[4..12].try_into().unwrap();
        let mut ready = if self.key.is_some_and(|k| k != key) {
            self.finish()
        } else {
            vec![]
        };
        self.key = Some(key);
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let first = self.count == 0;
        if self
            .previous
            .is_some_and(|last| sequence != last.wrapping_add(1))
        {
            self.bad = true
        }
        self.previous = Some(sequence);
        self.count += 1;
        self.bytes += packet.len();
        if self.bytes > 8 * 1024 * 1024 || self.count > 8192 {
            self.bad = true;
            self.packets.clear();
        }
        if self.codec == Codec::Mjpeg {
            if packet.len() < 20 || (first && packet.get(13..16) != Some(&[0, 0, 0])) {
                self.bad = true
            }
            self.jpeg_end = packet[1] & 128 != 0;
        } else {
            let kind = packet[12] & 31;
            if first && kind != 7 {
                self.bad = true
            }
            match kind {
                28 if packet.len() >= 15 => {
                    let fu = packet[13];
                    let start = fu & 128 != 0;
                    let end = fu & 64 != 0;
                    if fu & 32 != 0
                        || !(1..=5).contains(&(fu & 31))
                        || (start && end)
                        || (start == self.fragment)
                    {
                        self.bad = true
                    }
                    if start {
                        self.fragment = true
                    }
                    if end {
                        self.fragment = false;
                        self.vcl = true
                    }
                }
                1..=23 => {
                    if self.fragment {
                        self.bad = true
                    }
                    if kind <= 5 {
                        self.vcl = true
                    }
                }
                _ => self.bad = true,
            }
        }
        if !self.bad {
            self.packets.push(packet)
        }
        if self.codec == Codec::Mjpeg && self.jpeg_end {
            ready.extend(self.finish())
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(sequence: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, 96];
        p.extend(sequence.to_be_bytes());
        p.extend(timestamp.to_be_bytes());
        p.extend([0, 0, 0, 1]);
        p.extend(payload);
        p
    }
    #[test]
    fn complete_picture_and_sequence_wrap() {
        let mut f = Frames::new(Codec::H264);
        assert!(f.push(packet(65535, 1, &[0x67, 1])).is_empty());
        f.push(packet(0, 1, &[0x68, 1]));
        f.push(packet(1, 1, &[0x65, 1]));
        let out = f.push(packet(2, 2, &[0x67, 1]));
        assert_eq!(out.len(), 3);
        assert!(out[..2].iter().all(|p| p[1] == 96));
        assert_eq!(out[2][1], 224);
    }
    #[test]
    fn missing_fragment_drops_picture_and_recovers() {
        let mut f = Frames::new(Codec::H264);
        f.push(packet(0, 1, &[0x67, 1]));
        f.push(packet(1, 1, &[0x7c, 0x85, 1]));
        f.push(packet(3, 1, &[0x7c, 0x45, 1]));
        assert!(f.push(packet(4, 2, &[0x67, 1])).is_empty());
        f.push(packet(5, 2, &[0x65, 1]));
        assert_eq!(f.finish().len(), 2);
        assert_eq!(f.discarded, 1);
    }
    #[test]
    fn partial_nal_is_not_forwarded() {
        let mut f = Frames::new(Codec::H264);
        f.push(packet(1, 1, &[0x67, 1]));
        f.push(packet(2, 1, &[0x7c, 0x85, 1]));
        assert!(f.finish().is_empty());
        assert_eq!(f.discarded, 1);
    }
    #[test]
    fn jpeg_missing_first_fragment_is_rejected() {
        let mut f = Frames::new(Codec::Mjpeg);
        let mut p = packet(1, 1, &[0, 0, 0, 5, 1, 50, 80, 45, 0xff, 0xd9]);
        p[1] = 154;
        assert!(f.push(p).is_empty());
        assert_eq!(f.discarded, 1);
    }
    #[test]
    fn repairs_declared_length_and_rejects_truncated_capture() {
        let mac = [0, 0x21, 0x21, 1, 2, 3];
        let mut rtp = packet(1, 1, &[0x67, 1, 2, 3]);
        rtp[1] = 26;
        let mut frame = vec![0; 14 + 20 + 8];
        frame[6..12].copy_from_slice(&mac);
        frame[12..14].copy_from_slice(&[8, 0]);
        frame[14] = 0x45;
        frame[23] = 17;
        frame[16..18].copy_from_slice(&((28 + rtp.len() - 2) as u16).to_be_bytes());
        frame[36..38].copy_from_slice(&5004u16.to_be_bytes());
        frame[38..40].copy_from_slice(&((8 + rtp.len() - 2) as u16).to_be_bytes());
        frame.extend(&rtp);
        rtp[1] = 96;
        assert_eq!(extract(&frame, mac, 5004, Codec::H264), Some(rtp));
        frame.pop();
        assert!(extract(&frame, mac, 5004, Codec::H264).is_none());
    }
}
