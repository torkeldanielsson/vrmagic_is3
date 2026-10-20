//! One process owns control, watchdog feeding and the optional RTP repair relay.
mod capture;
mod control;
mod media;
mod network;
mod session;
mod watchdog;

use anyhow::{Context, Result, ensure};
use capture::Capture;
use clap::{Args, Parser, Subcommand};
use control::Camera;
use media::{Codec, Frames};
use network::Interface;
use serde_json::json;
use std::{
    fs::OpenOptions,
    io::Write,
    net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use watchdog::Watchdog;

#[derive(Parser)]
#[command(
    version,
    about = "Control VRmagic IS3 cameras and deliver usable RTP video"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read firmware, stream state and camera clock.
    Status(Target),
    /// Keep an idle camera alive by feeding its external watchdog.
    Keepalive(Run),
    /// Stream video and feed the watchdog until Ctrl+C.
    Stream(Stream),
}

#[derive(Args)]
struct Target {
    /// Camera Ethernet MAC, for example 00:21:21:01:02:03.
    #[arg(long, value_parser = parse_mac)]
    camera: [u8; 6],
    /// Ethernet interface on the camera's LAN.
    #[arg(long)]
    interface: String,
    /// Write a command trace to this new file.
    #[arg(long)]
    log: Option<PathBuf>,
}

#[derive(Args)]
struct Run {
    #[command(flatten)]
    target: Target,
    /// Stop after this many seconds; otherwise run until Ctrl+C.
    #[arg(long, value_parser = positive_seconds)]
    seconds: Option<f64>,
}

#[derive(Args)]
struct Stream {
    #[command(flatten)]
    run: Run,
    /// Receiver hostname or unicast IPv4 address.
    #[arg(long, default_value = "127.0.0.1")]
    to: String,
    /// Receiver UDP port.
    #[arg(long, default_value_t = 5004, value_parser = parse_port)]
    port: u16,
    /// Camera-to-relay port; by default reserve a free local port.
    #[arg(long, value_parser = parse_port, conflicts_with = "direct")]
    receive_port: Option<u16>,
    #[arg(long, value_enum, default_value_t = Codec::H264)]
    codec: Codec,
    #[arg(long, default_value_t = 8)]
    bit_depth: u8,
    #[arg(long, default_value_t = 1920)]
    width: u16,
    #[arg(long, default_value_t = 1080)]
    height: u16,
    #[arg(long, default_value_t = 12.0)]
    bitrate_mbps: f64,
    /// Send MJPEG straight from camera to receiver, without a relay.
    #[arg(long, requires = "destination_mac")]
    direct: bool,
    /// Receiver's Ethernet MAC on the camera LAN (required for --direct).
    #[arg(long, value_parser = parse_mac, requires = "direct")]
    destination_mac: Option<[u8; 6]>,
    /// Reboot this camera first; required when changing H.264 to MJPEG.
    #[arg(long)]
    reset: bool,
    /// Write an SDP file describing the stream to this new path.
    #[arg(long)]
    sdp: Option<PathBuf>,
}

fn parse_mac(value: &str) -> Result<[u8; 6], String> {
    let parts: Vec<_> = value.split([':', '-']).collect();
    if parts.len() != 6 || parts.iter().any(|p| p.len() != 2) {
        return Err("Expected six MAC octets separated by colons or dashes".into());
    }
    let mut result = [0; 6];
    for (byte, text) in result.iter_mut().zip(parts) {
        *byte = u8::from_str_radix(text, 16).map_err(|_| "Invalid MAC octet")?;
    }
    if result == [0; 6] || result[0] & 1 != 0 {
        return Err("Expected a unicast MAC".into());
    }
    Ok(result)
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port: u16 = value.parse().map_err(|_| "Invalid port")?;
    if !(1024..=65534).contains(&port) || port == 50001 {
        return Err("Use a port in 1024..65534, excluding control port 50001".into());
    }
    Ok(port)
}

fn positive_seconds(value: &str) -> Result<f64, String> {
    let seconds: f64 = value.parse().map_err(|_| "Invalid duration")?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("Duration must be finite and positive".into());
    }
    Ok(seconds)
}

fn running(stop: &AtomicBool, started: Instant, limit: Option<f64>) -> bool {
    !stop.load(Ordering::Relaxed)
        && limit.is_none_or(|seconds| started.elapsed().as_secs_f64() < seconds)
}

fn keepalive(args: &Run, camera: &mut Camera, stop: &AtomicBool) -> Result<()> {
    let mut watchdog = Watchdog::default();
    let result = (|| {
        watchdog.start(camera)?;
        let started = Instant::now();
        let mut next = started;
        let mut previous = None;
        while running(stop, started, args.seconds) {
            if Instant::now() >= next {
                watchdog.feed(camera)?;
                let clock = check_clock(camera, &mut previous)?;
                println!(
                    "{}",
                    json!({"event":"watchdog","camera_seconds":clock,"feeds":watchdog.feeds})
                );
                next = Instant::now() + Duration::from_secs(10);
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    })();
    let release = watchdog.release(camera);
    if let Err(error) = &release {
        eprintln!("Watchdog release failed: {error:#}");
    }
    result.and(release)
}

fn check_clock(camera: &mut Camera, previous: &mut Option<f64>) -> Result<f64> {
    let clock = camera.clock()?;
    ensure!(
        previous.is_none_or(|last| clock + 1.0 >= last),
        "Camera rebooted during this session"
    );
    *previous = Some(clock);
    Ok(clock)
}

fn stream(
    args: &Stream,
    camera: &mut Camera,
    interface: &Interface,
    stop: &AtomicBool,
) -> Result<()> {
    let profile = session::Profile {
        codec: args.codec,
        bit_depth: args.bit_depth,
        width: args.width,
        height: args.height,
        bitrate: args.bitrate_mbps,
    };
    profile.validate()?;
    ensure!(
        !args.direct || args.codec == Codec::Mjpeg,
        "Direct H.264 has truncated packets; use the relay or --codec mjpeg --direct"
    );
    let destination = (args.to.as_str(), args.port)
        .to_socket_addrs()?
        .find_map(|address| {
            if let SocketAddr::V4(v4) = address {
                Some(*v4.ip())
            } else {
                None
            }
        })
        .context("Receiver has no IPv4 address")?;
    ensure!(
        !destination.is_multicast() && !destination.is_unspecified() && !destination.is_broadcast(),
        "Receiver must have a unicast IPv4 address"
    );
    ensure!(
        !args.direct || !destination.is_loopback(),
        "Direct receiver must be on the camera LAN"
    );
    // Where the camera sends video: the receiver itself, or this machine.
    let video_destination = if args.direct {
        destination
    } else {
        interface
            .ipv4
            .context("The relay needs an IPv4 address on the interface")?
    };
    // Reserve the inbound port before configuring the camera. The raw capture
    // sees the full Ethernet frames, including bytes the normal UDP API loses.
    let receiver = if args.direct {
        None
    } else {
        Some(UdpSocket::bind((
            video_destination,
            args.receive_port.unwrap_or(0),
        ))?)
    };
    let receive_port = receiver
        .as_ref()
        .map(|s| s.local_addr().map(|a| a.port()))
        .transpose()?
        .unwrap_or(args.port);
    ensure!(
        args.direct
            || (receive_port != 50001
                && (!(destination.is_loopback() || destination == video_destination)
                    || receive_port != args.port)),
        "Camera-to-relay port must differ from the local output/control port"
    );
    let mut capture = if args.direct {
        None
    } else {
        Some(Capture::open(interface, args.run.target.camera)?)
    };
    let relay = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    if let Some(path) = &args.sdp {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?
            .write_all(session::sdp(destination, args.port, args.codec).as_bytes())?;
    }
    if args.reset {
        println!("{}", json!({"event":"reset"}));
        camera.reboot()?;
    }
    let mut watchdog = Watchdog::default();
    let mut changed = false;
    let result = (|| {
        watchdog.start(camera)?;
        changed = true;
        session::configure(
            camera,
            &profile,
            video_destination,
            args.destination_mac.unwrap_or(interface.mac),
            receive_port,
        )?;
        println!(
            "{}",
            json!({"event":"streaming","codec":format!("{:?}",args.codec),
            "bit_depth":args.bit_depth,"to":destination.to_string(),"port":args.port,
            "direct":args.direct,"reencoding":false,"watchdog_interval_seconds":10})
        );
        let started = Instant::now();
        let mut next_feed = started + Duration::from_secs(10);
        let mut next_status = started;
        let mut last_video = started;
        let mut previous_clock = None;
        let mut frames = Frames::new(args.codec);
        let mut packets = 0u64;
        let mut pictures = 0u64;
        while running(stop, started, args.run.seconds) {
            let now = Instant::now();
            if now >= next_feed {
                watchdog.feed(camera)?;
                next_feed = Instant::now() + Duration::from_secs(10);
            }
            if now >= next_status {
                let clock = check_clock(camera, &mut previous_clock)?;
                println!(
                    "{}",
                    json!({"event":"status","elapsed":started.elapsed().as_secs(),
                    "camera_seconds":clock,"feeds":watchdog.feeds,
                    "forwarded_packets":packets,"forwarded_frames":pictures,
                    "discarded_frames":frames.discarded})
                );
                next_status = Instant::now() + Duration::from_secs(30);
            }
            if let Some(capture) = &mut capture {
                ensure!(
                    last_video.elapsed() < Duration::from_secs(15),
                    "No valid camera video for 15 seconds"
                );
                if let Some(frame) = capture.receive()?
                    && let Some(packet) =
                        media::extract(frame, args.run.target.camera, receive_port, args.codec)
                {
                    last_video = Instant::now();
                    for outgoing in frames.push(packet) {
                        relay.send_to(&outgoing, (destination, args.port))?;
                        packets += 1;
                        pictures += u64::from(outgoing[1] & 128 != 0);
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(100));
            }
        }
        // A final frame can be partial when the user stops; do not emit it.
        println!(
            "{}",
            json!({"event":"finished","elapsed":started.elapsed().as_secs_f64(),
            "forwarded_packets":packets,"forwarded_frames":pictures,"feeds":watchdog.feeds,
            "discarded_frames":frames.discarded,"direct":args.direct})
        );
        Ok(())
    })();
    let suspend = if changed {
        (|| {
            if camera.state()? == 0x31 {
                camera.transition(0x40, 0x41)?;
            }
            Ok(())
        })()
    } else {
        Ok(())
    };
    if let Err(error) = &suspend {
        eprintln!("Stopping video failed: {error:#}");
    }
    let release = watchdog.release(camera);
    if let Err(error) = &release {
        eprintln!("Watchdog release failed: {error:#}");
    }
    result.and(suspend).and(release)
}

fn run() -> Result<()> {
    let args = Cli::parse();
    let target = match &args.command {
        Command::Status(target) => target,
        Command::Keepalive(run) => &run.target,
        Command::Stream(stream) => &stream.run.target,
    };
    let interface = network::interface(&target.interface)?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    let mut camera = Camera::connect(target.camera, interface.index, target.log.as_deref())?;
    match &args.command {
        Command::Status(_) => {
            println!(
                "{}",
                json!({"event":"status","firmware":camera.firmware()?,
                "state":format!("{:02x}",camera.state()?),"camera_seconds":camera.clock()?})
            );
            Ok(())
        }
        Command::Keepalive(run) => keepalive(run, &mut camera, &stop),
        Command::Stream(args) => stream(args, &mut camera, &interface, &stop),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("is3: {error:#}");
        std::process::exit(1);
    }
}
