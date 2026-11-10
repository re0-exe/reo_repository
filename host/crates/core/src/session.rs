//! One streaming session: from "a tablet is attached" to "the link dropped".

use crate::display::DisplayMode;
use crate::encode::default_bitrate_kbps;
use protocol::{Config, Hello};

/// Used when the tablet reports no refresh rates at all.
const FALLBACK_FPS: u16 = 60;
/// Largest frame edge a hardware HEVC encoder is expected to accept.
const MAX_DIMENSION: u32 = 8192;

/// The tablet's display must be something the encoder can take: NV12 needs
/// even dimensions.
pub fn validate_hello(hello: &Hello) -> Result<(), String> {
    let ok = |v: u32| v > 0 && v % 2 == 0 && v <= MAX_DIMENSION;
    if ok(hello.width) && ok(hello.height) {
        Ok(())
    } else {
        Err(format!("tablet reported an unusable resolution {}x{}", hello.width, hello.height))
    }
}

fn usable_rates(hello: &Hello) -> Vec<u16> {
    let mut rates: Vec<u16> = hello.refresh_hz.iter().copied().filter(|hz| *hz > 0).collect();
    rates.sort_unstable();
    rates.dedup();
    if rates.is_empty() {
        rates.push(FALLBACK_FPS);
    }
    rates
}

/// The monitor Windows is shown: the tablet's native resolution at every
/// refresh rate the panel supports.
pub fn display_mode(hello: &Hello) -> DisplayMode {
    DisplayMode { width: hello.width, height: hello.height, refresh_hz: usable_rates(hello) }
}

/// Picks the stream parameters for a tablet. The frame rate is the preferred
/// one when the panel supports it, otherwise the closest lower rate, otherwise
/// the panel's lowest.
pub fn choose_config(hello: &Hello, preferred_fps: u16) -> Config {
    let rates = usable_rates(hello);
    let fps = rates.iter().copied().filter(|hz| *hz <= preferred_fps).max().unwrap_or(rates[0]);
    Config {
        width: hello.width,
        height: hello.height,
        fps,
        codec: 0,
        hdr: false,
        profile: 0,
        bitrate_kbps: default_bitrate_kbps(hello.width, hello.height, fps),
    }
}

/// Why a session cannot run at all, as opposed to having been interrupted.
#[derive(Debug)]
pub enum SessionError {
    DriverMissing,
    NoHardwareEncoder,
    /// The tablet app speaks another protocol version.
    VersionMismatch,
    /// Toggling the virtual display failed, typically for lack of elevation.
    Display(String),
    /// The driver started but its monitor did not show up in Windows.
    DisplayNotFound(String),
    Other(String),
}

#[derive(Debug)]
pub enum SessionEnd {
    /// The link dropped (cable pulled, app closed). Try again.
    Disconnected,
    /// Capture or encoding failed mid-stream (GPU reset, lock screen). The
    /// session can be started again.
    Interrupted(String),
    /// The user asked the program to exit.
    Shutdown,
    Fatal(SessionError),
}

#[cfg(windows)]
pub use win::run_session;

#[cfg(windows)]
mod win {
    use super::*;
    use crate::adb::Adb;
    use crate::capture::{CaptureEvent, CapturedFrame, DesktopDuplicator};
    use crate::display::{DisplayError, VirtualDisplay};
    use crate::encode::{EncodeError, EncodedFrame, EncoderConfig, MfHevcEncoder, VideoEncoder};
    use crate::gate::SendGate;
    use crate::transport::{AdbTransport, Transport};
    use protocol::{read_message, write_message, Channel, Message, ProtocolError, ReadError, PORT};
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// The app needs a moment to start listening after `am start`.
    const HANDSHAKE_ATTEMPTS: u32 = 40;
    const HANDSHAKE_INTERVAL: Duration = Duration::from_millis(500);
    const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
    /// Frames encoded but not yet on the wire before new ones are skipped.
    const MAX_IN_FLIGHT: usize = 2;
    /// Capture wait; also bounds how quickly a dropped link is noticed.
    const CAPTURE_TIMEOUT_MS: u32 = 16;
    const PING_INTERVAL: Duration = Duration::from_secs(1);
    const STATS_INTERVAL: Duration = Duration::from_secs(5);
    /// A mode change leaves the output unavailable for a short while.
    const REOPEN_ATTEMPTS: u32 = 40;
    const REOPEN_INTERVAL: Duration = Duration::from_millis(250);

    fn handshake(transport: &AdbTransport) -> Result<(TcpStream, Hello), ReadError> {
        let mut control = transport.open(Channel::Control)?;
        control.set_read_timeout(Some(HELLO_TIMEOUT))?;
        let hello = loop {
            if let (_, Message::Hello(h)) = read_message(&mut control)? {
                break h;
            }
        };
        control.set_read_timeout(None)?;
        Ok((control, hello))
    }

    /// Opens capture on the virtual monitor, waiting for it to become
    /// available. `keep_trying` lets the caller wait out situations with no
    /// fixed duration, such as the lock screen, where duplication is denied.
    fn open_duplicator(display: &VirtualDisplay, keep_trying: impl Fn() -> bool) -> Result<DesktopDuplicator, String> {
        let mut last = String::new();
        let mut attempt = 0;
        while attempt < REOPEN_ATTEMPTS || keep_trying() {
            attempt += 1;
            let opened = display
                .output()
                .map_err(|e| e.to_string())
                .and_then(|o| DesktopDuplicator::new(&o).map_err(|e| e.to_string()));
            match opened {
                Ok(d) => return Ok(d),
                Err(e) => last = e,
            }
            std::thread::sleep(REOPEN_INTERVAL);
        }
        Err(last)
    }

    pub fn run_session(adb: &Adb, serial: &str, preferred_fps: u16) -> SessionEnd {
        if adb.forward(serial, PORT).is_err() || adb.start_app(serial).is_err() {
            return SessionEnd::Disconnected;
        }
        let end = stream(preferred_fps);
        let _ = adb.remove_forward(serial, PORT);
        end
    }

    fn stream(preferred_fps: u16) -> SessionEnd {
        let transport = AdbTransport { port: PORT };

        // A connect through the adb tunnel succeeds even before the app
        // listens; the stream then ends immediately. Both cases mean "retry".
        let mut attempt = 0;
        let (control, hello) = loop {
            match handshake(&transport) {
                Ok(pair) => break pair,
                Err(ReadError::Protocol(ProtocolError::BadVersion(_))) => {
                    return SessionEnd::Fatal(SessionError::VersionMismatch)
                }
                Err(_) if attempt + 1 < HANDSHAKE_ATTEMPTS => {
                    attempt += 1;
                    std::thread::sleep(HANDSHAKE_INTERVAL);
                }
                Err(_) => return SessionEnd::Disconnected,
            }
        };
        if let Err(e) = validate_hello(&hello) {
            return SessionEnd::Fatal(SessionError::Other(e));
        }

        let config = choose_config(&hello, preferred_fps);
        println!(
            "Tablet: {}x{} @ {:?} Hz -> {} fps, {} Mbps",
            hello.width,
            hello.height,
            hello.refresh_hz,
            config.fps,
            config.bitrate_kbps / 1000
        );
        let mut control_writer = match control.try_clone() {
            Ok(w) => w,
            Err(_) => return SessionEnd::Disconnected,
        };
        if write_message(&mut control_writer, 0, &Message::Config(config.clone())).is_err() {
            return SessionEnd::Disconnected;
        }

        // The tablet waits for the video channel right after `Config`; open
        // it before the display is brought up, which can take seconds.
        let video = match transport.open(Channel::Video) {
            Ok(v) => v,
            Err(_) => return SessionEnd::Disconnected,
        };
        let display = match VirtualDisplay::enable(&display_mode(&hello)) {
            Ok(d) => d,
            Err(DisplayError::DriverMissing) => return SessionEnd::Fatal(SessionError::DriverMissing),
            Err(e @ DisplayError::OutputNotFound(_)) => {
                return SessionEnd::Fatal(SessionError::DisplayNotFound(e.to_string()))
            }
            Err(e) => return SessionEnd::Fatal(SessionError::Display(e.to_string())),
        };

        let stop = Arc::new(AtomicBool::new(false));
        let keyframe_wanted = Arc::new(AtomicBool::new(false));
        let ping_sent_us = Arc::new(AtomicU64::new(0));
        let rtt_us = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(SendGate::new(MAX_IN_FLIGHT));

        // Control reader: keyframe requests and ping replies from the tablet.
        {
            let (stop, keyframe_wanted, ping_sent_us, rtt_us) =
                (stop.clone(), keyframe_wanted.clone(), ping_sent_us.clone(), rtt_us.clone());
            let mut control = control;
            std::thread::spawn(move || {
                loop {
                    match read_message(&mut control) {
                        Ok((_, Message::RequestKeyframe)) => keyframe_wanted.store(true, Ordering::Relaxed),
                        Ok((_, Message::Pong)) => {
                            let sent = ping_sent_us.load(Ordering::Relaxed);
                            rtt_us.store(crate::capture::now_us().saturating_sub(sent), Ordering::Relaxed);
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                stop.store(true, Ordering::Relaxed);
            });
        }

        // Video writer: keeps socket back-pressure off the capture thread.
        let (tx, rx) = mpsc::channel::<(u64, Message)>();
        {
            let (stop, gate) = (stop.clone(), gate.clone());
            let mut video = video;
            std::thread::spawn(move || {
                for (pts, msg) in rx {
                    let is_frame = matches!(msg, Message::VideoFrame { .. });
                    let result = write_message(&mut video, pts, &msg);
                    if is_frame {
                        gate.release();
                    }
                    if result.is_err() {
                        break;
                    }
                }
                stop.store(true, Ordering::Relaxed);
            });
        }

        let running = || !stop.load(Ordering::Relaxed) && !crate::shutdown::requested();
        let mut duplicator = match open_duplicator(&display, || false) {
            Ok(d) => d,
            Err(e) => return SessionEnd::Fatal(SessionError::Other(e)),
        };
        let mut encoder: Option<MfHevcEncoder> = None;
        let mut sent_parameter_sets: Vec<u8> = Vec::new();
        // A frame that was skipped or is still inside the encoder must reach
        // the tablet even if the screen then stops changing.
        let mut pending = false;
        let mut frames = 0u64;
        let mut last_ping = Instant::now();
        let mut last_stats = Instant::now();
        println!("Yayın başladı.");

        loop {
            if crate::shutdown::requested() {
                return SessionEnd::Shutdown;
            }
            if stop.load(Ordering::Relaxed) {
                return SessionEnd::Disconnected;
            }
            if last_ping.elapsed() >= PING_INTERVAL {
                last_ping = Instant::now();
                ping_sent_us.store(crate::capture::now_us(), Ordering::Relaxed);
                if write_message(&mut control_writer, 0, &Message::Ping).is_err() {
                    return SessionEnd::Disconnected;
                }
            }
            if last_stats.elapsed() >= STATS_INTERVAL {
                let secs = last_stats.elapsed().as_secs_f64();
                println!(
                    "{:.0} fps | USB ping {:.1} ms | atlanan frame {}",
                    frames as f64 / secs,
                    rtt_us.load(Ordering::Relaxed) as f64 / 1000.0,
                    gate.dropped()
                );
                frames = 0;
                last_stats = Instant::now();
            }

            let event = match duplicator.next(CAPTURE_TIMEOUT_MS) {
                Ok(e) => e,
                Err(e) => return SessionEnd::Interrupted(e.to_string()),
            };
            // A keyframe request must be answered even when the screen is
            // still: the tablet shows nothing until it gets one.
            let resend = pending || keyframe_wanted.load(Ordering::Relaxed);
            let frame: Option<CapturedFrame> = match event {
                CaptureEvent::Frame(f) => Some(f),
                CaptureEvent::Timeout if resend => duplicator.last_frame(),
                CaptureEvent::Timeout => None,
                CaptureEvent::ModeChanged => {
                    // Resolution, orientation or refresh rate changed in
                    // Windows: rebuild capture, and the encoder with it.
                    // While the lock screen or a UAC prompt is up, duplication
                    // is refused; keep waiting for as long as the link lives.
                    encoder = None;
                    duplicator = match open_duplicator(&display, running) {
                        Ok(d) => d,
                        Err(_) if crate::shutdown::requested() => return SessionEnd::Shutdown,
                        Err(_) if stop.load(Ordering::Relaxed) => return SessionEnd::Disconnected,
                        Err(e) => return SessionEnd::Interrupted(e),
                    };
                    continue;
                }
            };

            let mut output: Option<EncodedFrame> = None;
            if let Some(frame) = frame {
                if encoder.as_ref().is_some_and(|e| !e.matches(frame.width, frame.height)) {
                    encoder = None;
                }
                if encoder.is_none() {
                    let cfg = EncoderConfig {
                        width: frame.width,
                        height: frame.height,
                        fps: config.fps,
                        bitrate_kbps: default_bitrate_kbps(frame.width, frame.height, config.fps),
                    };
                    encoder = match MfHevcEncoder::new(duplicator.device(), &cfg) {
                        Ok(e) => Some(e),
                        Err(EncodeError::NoHardwareEncoder) => {
                            return SessionEnd::Fatal(SessionError::NoHardwareEncoder)
                        }
                        Err(e) => return SessionEnd::Interrupted(e.to_string()),
                    };
                    sent_parameter_sets.clear();
                }
                let enc = encoder.as_mut().expect("encoder was just created");
                if gate.try_acquire() {
                    if keyframe_wanted.swap(false, Ordering::Relaxed) {
                        enc.request_keyframe();
                    }
                    pending = false;
                    match enc.encode(&frame) {
                        Ok(Some(f)) => output = Some(f),
                        // Still inside the encoder; collected by `poll` below.
                        Ok(None) => gate.release(),
                        Err(e) => return SessionEnd::Interrupted(e.to_string()),
                    }
                } else {
                    pending = true;
                }
            } else if let Some(enc) = encoder.as_mut() {
                // Only take a finished frame out of the encoder when it can be
                // sent: an encoded frame must never be discarded, later frames
                // reference it.
                if gate.try_acquire() {
                    match enc.poll() {
                        Ok(Some(f)) => output = Some(f),
                        Ok(None) => gate.release(),
                        Err(e) => return SessionEnd::Interrupted(e.to_string()),
                    }
                }
            }

            let (Some(out), Some(enc)) = (output, encoder.as_mut()) else { continue };
            if out.keyframe {
                let ps = enc.parameter_sets();
                if !ps.is_empty() && ps != sent_parameter_sets {
                    if tx.send((out.pts_us, Message::VideoConfig(ps.clone()))).is_err() {
                        return SessionEnd::Disconnected;
                    }
                    sent_parameter_sets = ps;
                }
            }
            if sent_parameter_sets.is_empty() {
                // The decoder cannot start on this frame; ask for one it can.
                gate.release();
                enc.request_keyframe();
                pending = true;
                continue;
            }
            frames += 1;
            if tx.send((out.pts_us, Message::VideoFrame { keyframe: out.keyframe, data: out.data })).is_err() {
                return SessionEnd::Disconnected;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(hz: &[u16]) -> Hello {
        Hello { version: 1, width: 2800, height: 1752, refresh_hz: hz.to_vec(), caps: 1, dpi: 266 }
    }

    #[test]
    fn config_prefers_requested_fps() {
        assert_eq!(choose_config(&hello(&[60, 120]), 120).fps, 120);
    }

    #[test]
    fn config_falls_back_to_lower() {
        assert_eq!(choose_config(&hello(&[60, 90]), 120).fps, 90);
    }

    #[test]
    fn config_falls_back_to_min() {
        assert_eq!(choose_config(&hello(&[144, 165]), 120).fps, 144);
    }

    #[test]
    fn config_without_reported_rates_uses_60() {
        assert_eq!(choose_config(&hello(&[]), 120).fps, 60);
    }

    #[test]
    fn config_uses_hello_resolution() {
        let c = choose_config(&hello(&[60, 120]), 120);
        assert_eq!((c.width, c.height), (2800, 1752));
        assert_eq!((c.codec, c.hdr, c.profile), (0, false, 0));
        assert_eq!(c.bitrate_kbps, 82_414);
    }

    #[test]
    fn display_mode_lists_only_usable_rates() {
        let m = display_mode(&hello(&[60, 120, 0]));
        assert_eq!(m, DisplayMode { width: 2800, height: 1752, refresh_hz: vec![60, 120] });
        assert_eq!(display_mode(&hello(&[])).refresh_hz, vec![60]);
    }

    #[test]
    fn rejects_unusable_resolution() {
        assert!(validate_hello(&hello(&[60])).is_ok());
        assert!(validate_hello(&Hello { width: 0, ..hello(&[60]) }).is_err());
        assert!(validate_hello(&Hello { width: 2801, ..hello(&[60]) }).is_err());
        assert!(validate_hello(&Hello { height: 20_000, ..hello(&[60]) }).is_err());
    }
}
