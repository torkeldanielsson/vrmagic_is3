//! Linux interface enumeration and a camera-filtered AF_PACKET socket.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{CStr, CString},
    io,
    net::Ipv4Addr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

pub struct Interface {
    pub index: u32,
    pub ipv4: Ipv4Addr,
    pub mac: [u8; 6],
}

pub fn interface(name: &str) -> Result<Interface> {
    let name_c = CString::new(name)?;
    // SAFETY: NUL-terminated interface name; pointer valid for the call.
    let index = unsafe { libc::if_nametoindex(name_c.as_ptr()) };
    ensure!(index != 0, "Unknown interface {name}");
    let mut head = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a linked list and sets head on success.
    ensure!(
        unsafe { libc::getifaddrs(&mut head) } == 0,
        "getifaddrs: {}",
        io::Error::last_os_error()
    );
    struct Addresses(*mut libc::ifaddrs);
    impl Drop for Addresses {
        fn drop(&mut self) {
            // SAFETY: this is the list returned by getifaddrs, freed once.
            unsafe { libc::freeifaddrs(self.0) }
        }
    }
    let guard = Addresses(head);
    let mut current = guard.0;
    let mut ip = None;
    let mut mac = None;
    while !current.is_null() {
        // SAFETY: nodes and family-specific addresses belong to the live list.
        unsafe {
            let item = &*current;
            if CStr::from_ptr(item.ifa_name).to_bytes() == name.as_bytes()
                && !item.ifa_addr.is_null()
            {
                match i32::from((*item.ifa_addr).sa_family) {
                    libc::AF_INET if ip.is_none() => {
                        let addr = &*(item.ifa_addr as *const libc::sockaddr_in);
                        ip = Some(Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()));
                    }
                    libc::AF_PACKET => {
                        let addr = &*(item.ifa_addr as *const libc::sockaddr_ll);
                        if addr.sll_halen == 6 && addr.sll_hatype == 1 {
                            mac = Some(addr.sll_addr[..6].try_into()?);
                        }
                    }
                    _ => {}
                }
            }
            current = item.ifa_next;
        }
    }
    Ok(Interface {
        index,
        ipv4: ip.context("Interface needs an IPv4 address")?,
        mac: mac.context("Choose an Ethernet interface")?,
    })
}

pub struct Capture(OwnedFd);

impl Capture {
    pub fn open(index: u32, mac: [u8; 6]) -> Result<Self> {
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
        // SAFETY: fd is a newly created, valid descriptor.
        let capture = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        // SAFETY: zero is a valid initial representation for sockaddr_ll.
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = u16::to_be(0x0800);
        addr.sll_ifindex = index.try_into()?;
        // SAFETY: address points to a sockaddr_ll with its exact byte length.
        ensure!(
            unsafe {
                libc::bind(
                    fd,
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of_val(&addr) as libc::socklen_t,
                )
            } == 0,
            "Capture bind: {}",
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

    pub fn receive(&self, buffer: &mut [u8]) -> Result<Option<usize>> {
        // SAFETY: writable slice is valid for the supplied length.
        let n = unsafe {
            libc::recv(
                self.0.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if n >= 0 {
            return Ok(Some(n as usize));
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
