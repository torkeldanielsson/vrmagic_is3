//! A camera-filtered AF_PACKET socket.
use crate::network::Interface;
use anyhow::{Context, Result, ensure};
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

pub struct Capture {
    socket: OwnedFd,
    buffer: Vec<u8>,
}

impl Capture {
    pub fn open(interface: &Interface, mac: [u8; 6]) -> Result<Self> {
        // SAFETY: socket has only integer arguments; successful fd becomes owned.
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                u16::to_be(0x0800).into(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context(
                "Raw capture needs CAP_NET_RAW. Run `sudo $(command -v is3) stream ...`, or grant the installed binary cap_net_raw");
        }
        let capture = Self {
            // SAFETY: fd is a newly created, valid descriptor.
            socket: unsafe { OwnedFd::from_raw_fd(fd) },
            buffer: vec![0; 65536],
        };
        // SAFETY: zero is a valid initial representation for sockaddr_ll.
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = u16::to_be(0x0800);
        addr.sll_ifindex = interface.index.try_into()?;
        // SAFETY: address points to a sockaddr_ll with its exact byte length.
        ensure!(
            unsafe {
                libc::bind(
                    fd,
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of_val(&addr) as libc::socklen_t,
                )
            } == 0,
            "Capture on {}: {}",
            interface.name,
            io::Error::last_os_error()
        );
        let filter = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
        let mut code = [
            filter(0x20, 0, 0, 6), // Ethernet source MAC, first four bytes.
            filter(0x15, 0, 3, u32::from_be_bytes(mac[..4].try_into()?)),
            filter(0x28, 0, 0, 10),
            filter(0x15, 0, 1, u16::from_be_bytes(mac[4..].try_into()?).into()),
            filter(0x06, 0, 0, 65535),
            filter(0x06, 0, 0, 0),
        ];
        let program = libc::sock_fprog {
            len: code.len() as u16,
            filter: code.as_mut_ptr(),
        };
        // SAFETY: kernel copies this valid filter and length during setsockopt.
        ensure!(
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ATTACH_FILTER,
                    &program as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&program) as _,
                )
            } == 0,
            "Capture filter: {}",
            io::Error::last_os_error()
        );
        let timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 200000,
        };
        let buffer: i32 = 8 * 1024 * 1024;
        // SAFETY: valid values and exact lengths; kernel may cap the buffer size.
        unsafe {
            ensure!(
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &timeout as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&timeout) as _
                ) == 0,
                "Capture timeout: {}",
                io::Error::last_os_error()
            );
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &buffer as *const _ as *const libc::c_void,
                std::mem::size_of_val(&buffer) as _,
            );
        }
        Ok(capture)
    }

    pub fn receive(&mut self) -> Result<Option<&[u8]>> {
        // SAFETY: writable buffer is valid for the supplied length.
        let n = unsafe {
            libc::recv(
                self.socket.as_raw_fd(),
                self.buffer.as_mut_ptr().cast(),
                self.buffer.len(),
                0,
            )
        };
        if n >= 0 {
            return Ok(Some(&self.buffer[..n as usize]));
        }
        let error = io::Error::last_os_error();
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ) {
            Ok(None)
        } else {
            Err(error.into())
        }
    }
}
