//! ACCP framing. All camera operations are synchronous: the firmware
//! shares its ISP argument pool across commands, so no background feeder races it.
use anyhow::{Context, Result, bail, ensure};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions, TryLockError},
    hash::{BuildHasher, RandomState},
    io::{ErrorKind, Write},
    net::{Ipv6Addr, SocketAddrV6, UdpSocket},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub struct Camera {
    socket: UdpSocket,
    identifier: u32,
    _lock: File,
    log: Option<File>,
}

pub fn address(mac: [u8; 6]) -> Ipv6Addr {
    Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([mac[0] ^ 2, mac[1]]),
        u16::from_be_bytes([mac[2], 0xff]),
        u16::from_be_bytes([0xfe, mac[3]]),
        u16::from_be_bytes([mac[4], mac[5]]),
    )
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|x| format!("{x:02x}")).collect()
}

impl Camera {
    pub fn connect(mac: [u8; 6], index: u32, log: Option<&Path>) -> Result<Self> {
        // macOS gives each user its own temporary directory; a lock in /tmp
        // also excludes an `is3` run with sudo.
        let directory = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let path = directory.join(format!("vrmagic-is3-{}.lock", hex(&mac)));
        let mut options = OpenOptions::new();
        options.read(true);
        // Never follow a link planted in the shared directory.
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let lock = match options.clone().write(true).create_new(true).open(&path) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => options.open(&path),
            lock => lock,
        }
        .with_context(|| format!("Open lock file {}", path.display()))?;
        match lock.try_lock() {
            Err(TryLockError::WouldBlock) => {
                bail!("Camera already controlled by another local is3 process")
            }
            locked => locked?,
        }
        let socket = UdpSocket::bind("[::]:0")?;
        socket
            .connect(SocketAddrV6::new(address(mac), 50001, 0, index))
            .context("The camera is controlled over IPv6; enable it on the interface")?;
        Ok(Self {
            socket,
            // The camera answers a reused identifier with a stale reply, so
            // each session starts at a random one.
            identifier: RandomState::new().hash_one(Instant::now()) as u32,
            _lock: lock,
            log: log
                .map(|p| OpenOptions::new().write(true).create_new(true).open(p))
                .transpose()?,
        })
    }

    pub fn command(&mut self, opcode: u16, payload: &[u8], attempts: usize) -> Result<Vec<u8>> {
        ensure!(
            payload.len() <= 254 && (1..=20).contains(&attempts),
            "Invalid command bounds"
        );
        self.identifier = self.identifier.wrapping_add(1).max(1);
        let mut tx = self.identifier.to_be_bytes().to_vec();
        tx.extend(opcode.to_be_bytes());
        tx.extend((payload.len() as u16).to_be_bytes());
        tx.extend(payload);
        let mut rx = [0u8; 65536];
        for attempt in 0..attempts {
            let start = Instant::now();
            self.socket.send(&tx)?;
            let deadline = start + Duration::from_millis(300);
            let mut reply = None;
            while Instant::now() < deadline {
                self.socket.set_read_timeout(Some(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_micros(1)),
                ))?;
                match self.socket.recv(&mut rx) {
                    Ok(n) if n >= 4 && rx[..4] == tx[..4] => {
                        reply = Some(rx[..n].to_vec());
                        break;
                    }
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::TimedOut
                                | ErrorKind::WouldBlock
                                | ErrorKind::ConnectionRefused
                                | ErrorKind::ConnectionReset
                        ) =>
                    {
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            if let Some(log) = &mut self.log {
                writeln!(
                    log,
                    "{}",
                    serde_json::json!({"attempt":attempt,"tx":hex(&tx),"rx":reply.as_ref().map(|r|hex(r))})
                )?;
            }
            thread::sleep(Duration::from_millis(25).saturating_sub(start.elapsed()));
            if let Some(reply) = reply {
                ensure!(reply.len() >= 8, "Short ACCP reply");
                let status = u16::from_be_bytes([reply[4], reply[5]]);
                let length = usize::from(u16::from_be_bytes([reply[6], reply[7]]));
                ensure!(reply.len() == 8 + length, "ACCP reply length mismatch");
                ensure!(
                    status == 0,
                    "Camera command {opcode:04x} returned status {status:04x}"
                );
                return Ok(reply[8..].to_vec());
            }
            thread::sleep(Duration::from_millis(300));
        }
        bail!("No response to {opcode:04x}")
    }

    pub fn firmware(&mut self) -> Result<String> {
        Ok(hex(&self.command(0xc905, &[0x80, 0x0c, 0, 0], 20)?))
    }

    pub fn clock(&mut self) -> Result<f64> {
        let value = self.command(0xc220, &[], 3)?;
        ensure!(value.len() == 10, "Invalid clock response");
        let seconds = value[..6].iter().fold(0u64, |n, x| n * 256 + u64::from(*x));
        let nanos = u32::from_be_bytes(value[6..].try_into()?);
        ensure!(nanos < 1_000_000_000, "Invalid clock nanoseconds");
        Ok(seconds as f64 + f64::from(nanos) / 1e9)
    }

    pub fn read(&mut self, address: u16, count: u16, width: u8, logical: bool) -> Result<Vec<u8>> {
        ensure!(count > 0 && [8, 16, 32].contains(&width), "Invalid read");
        let command = if logical {
            match width {
                8 => 0xc501,
                16 => 0xc503,
                _ => 0xc505,
            }
        } else {
            ensure!(width == 16 && address < 0x7fff, "Unsupported physical read");
            0xc403
        };
        let bytes = usize::from(count) * usize::from(width / 8);
        ensure!(
            bytes <= 254 && address.is_multiple_of(u16::from(width / 8)),
            "Invalid read range"
        );
        let payload = [address.to_be_bytes(), (count - 1).to_be_bytes()].concat();
        let value = self.command(command, &payload, 10)?;
        ensure!(value.len() == bytes, "Read size mismatch");
        Ok(value)
    }

    pub fn write(&mut self, address: u16, data: &[u8], width: u8) -> Result<()> {
        ensure!([8, 16, 32].contains(&width), "Invalid write width");
        let step = usize::from(width / 8);
        ensure!(
            address >= 0x8000
                && usize::from(address).is_multiple_of(step)
                && !data.is_empty()
                && data.len() <= 250
                && data.len().is_multiple_of(step)
                && usize::from(address & 0x3ff) + data.len() <= 1024,
            "Invalid volatile write"
        );
        let mut payload = [
            address.to_be_bytes(),
            ((data.len() / step - 1) as u16).to_be_bytes(),
        ]
        .concat();
        payload.extend(data);
        self.command(
            match width {
                8 => 0xc500,
                16 => 0xc502,
                _ => 0xc504,
            },
            &payload,
            1,
        )?;
        Ok(())
    }

    pub fn state(&mut self) -> Result<u8> {
        self.command(0x8101, &[], 5)?
            .first()
            .copied()
            .context("Empty state reply")
    }

    pub fn transition(&mut self, request: u8, target: u8) -> Result<()> {
        self.command(0x8100, &[request, 0], 1)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.state()? == target {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("State transition to {target:02x} timed out")
    }

    pub fn reboot(&mut self) -> Result<()> {
        let before = self.clock()?;
        let started = Instant::now();
        // Exactly one reset request: a lost acknowledgement must not loop resets.
        let acknowledgement = self.command(0xc210, &[0x07, 0xd0, 0, 0], 1);
        while started.elapsed() < Duration::from_secs(30) {
            if let Ok(clock) = self.clock()
                && before + started.elapsed().as_secs_f64() - clock > 1.0
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(200));
        }
        bail!("No reboot observed; acknowledgement={acknowledgement:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eui64_control_address() {
        assert_eq!(
            address([0, 0x21, 0x21, 1, 2, 3]).to_string(),
            "fe80::221:21ff:fe01:203"
        );
    }
}
