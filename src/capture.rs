//! Whole Ethernet frames from the camera, including the bytes its H.264
//! packets leave outside the declared IP and UDP lengths.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "macos", windows))]
mod pcap;

#[cfg(target_os = "linux")]
pub use linux::Capture;
#[cfg(any(target_os = "macos", windows))]
pub use pcap::Capture;
