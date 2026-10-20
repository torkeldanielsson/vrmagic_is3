//! Capture through libpcap, or Npcap on Windows. The library is loaded when
//! the relay starts, so every other command runs without it.
use crate::network::Interface;
use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_void},
    ptr,
};

type Handle = *mut c_void;

#[repr(C)]
struct Program {
    length: c_uint,
    instructions: *mut c_void,
}

#[repr(C)]
struct Header {
    /// `struct timeval`, the size and alignment of two `long`s on macOS and Windows.
    timestamp: [c_long; 2],
    captured: u32,
    length: u32,
}

const ERROR_BUFFER: usize = 256;
const PERMISSION_DENIED: c_int = -8;
const ETHERNET: c_int = 1;

#[cfg(windows)]
const ACCESS: &str = "Npcap is installed for administrators only: run is3 as Administrator";
#[cfg(target_os = "macos")]
const ACCESS: &str = "Raw capture needs the BPF devices. Run `sudo $(command -v is3) stream ...`";

macro_rules! api {
    ($($name:ident: fn($($argument:ty),*) $(-> $result:ty)?;)*) => {
        struct Api {
            $($name: unsafe extern "C" fn($($argument),*) $(-> $result)?,)*
            _library: Library,
        }

        impl Api {
            fn load(library: Library) -> Result<Self> {
                // SAFETY: the signatures are libpcap's, and the library is
                // kept alive alongside the function pointers.
                unsafe {
                    Ok(Self {
                        $($name: *library.get(concat!("pcap_", stringify!($name)))?,)*
                        _library: library,
                    })
                }
            }
        }
    };
}

api! {
    create: fn(*const c_char, *mut c_char) -> Handle;
    set_timeout: fn(Handle, c_int) -> c_int;
    set_immediate_mode: fn(Handle, c_int) -> c_int;
    set_buffer_size: fn(Handle, c_int) -> c_int;
    activate: fn(Handle) -> c_int;
    datalink: fn(Handle) -> c_int;
    compile: fn(Handle, *mut Program, *const c_char, c_int, c_uint) -> c_int;
    setfilter: fn(Handle, *mut Program) -> c_int;
    freecode: fn(*mut Program);
    next_ex: fn(Handle, *mut *const Header, *mut *const u8) -> c_int;
    geterr: fn(Handle) -> *const c_char;
    close: fn(Handle);
}

#[cfg(windows)]
fn library() -> Result<Library> {
    use libloading::os::windows;
    let root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
    let path = std::path::Path::new(&root).join(r"System32\Npcap\wpcap.dll");
    // SAFETY: Npcap's initialisation has no requirements on the caller. The
    // flag makes wpcap.dll find Packet.dll in its own directory.
    unsafe { windows::Library::load_with_flags(path, windows::LOAD_WITH_ALTERED_SEARCH_PATH) }
        .map(Into::into)
        .context("The H.264 relay needs Npcap; install it from https://npcap.com")
}

#[cfg(target_os = "macos")]
fn library() -> Result<Library> {
    // SAFETY: libpcap's initialisation has no requirements on the caller.
    unsafe { Library::new("/usr/lib/libpcap.A.dylib") }.context("The H.264 relay needs libpcap")
}

pub struct Capture {
    api: Api,
    handle: Handle,
}

impl Capture {
    pub fn open(interface: &Interface, mac: [u8; 6]) -> Result<Self> {
        let api = Api::load(library()?)?;
        let device = if cfg!(windows) {
            format!(r"\Device\NPF_{}", interface.name)
        } else {
            interface.name.clone()
        };
        let device = CString::new(device)?;
        let mut error = [0; ERROR_BUFFER];
        // SAFETY: NUL-terminated device name and an error buffer of the size libpcap requires.
        let handle = unsafe { (api.create)(device.as_ptr(), error.as_mut_ptr()) };
        if handle.is_null() {
            // SAFETY: libpcap wrote a NUL-terminated message into the buffer.
            let message = unsafe { CStr::from_ptr(error.as_ptr()) };
            bail!("Capture: {}", message.to_string_lossy());
        }
        let capture = Self { api, handle };
        // SAFETY: a created, not yet activated handle; the options take plain integers.
        let status = unsafe {
            (capture.api.set_timeout)(handle, 200);
            (capture.api.set_immediate_mode)(handle, 1);
            (capture.api.set_buffer_size)(handle, 8 << 20);
            (capture.api.activate)(handle)
        };
        ensure!(
            status != PERMISSION_DENIED,
            "{ACCESS}: {}",
            capture.message()
        );
        capture.check(status)?;
        // SAFETY: an activated handle.
        let link = unsafe { (capture.api.datalink)(handle) };
        ensure!(link == ETHERNET, "Choose an Ethernet interface");
        let mac = mac.map(|x| format!("{x:02x}")).join(":");
        let filter = CString::new(format!("ether src {mac}"))?;
        let mut program = Program {
            length: 0,
            instructions: ptr::null_mut(),
        };
        // SAFETY: an activated handle and a NUL-terminated filter; the compiled
        // program is copied by setfilter and freed once.
        unsafe {
            capture.check((capture.api.compile)(
                handle,
                &mut program,
                filter.as_ptr(),
                1,
                c_uint::MAX,
            ))?;
            let installed = (capture.api.setfilter)(handle, &mut program);
            (capture.api.freecode)(&mut program);
            capture.check(installed)?;
        }
        Ok(capture)
    }

    pub fn receive(&mut self) -> Result<Option<&[u8]>> {
        let mut header = ptr::null();
        let mut data = ptr::null();
        // SAFETY: on success libpcap points both at its own buffer, which stays
        // valid until the next call; the returned slice borrows self until then.
        match unsafe { (self.api.next_ex)(self.handle, &mut header, &mut data) } {
            1 => Ok(Some(unsafe {
                std::slice::from_raw_parts(data, (*header).captured as usize)
            })),
            0 => Ok(None),
            _ => bail!("Capture: {}", self.message()),
        }
    }

    fn check(&self, status: c_int) -> Result<()> {
        ensure!(status >= 0, "Capture: {}", self.message());
        Ok(())
    }

    fn message(&self) -> String {
        // SAFETY: geterr returns a NUL-terminated string owned by the handle.
        unsafe { CStr::from_ptr((self.api.geterr)(self.handle)) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: the handle is closed exactly once.
        unsafe { (self.api.close)(self.handle) }
    }
}
