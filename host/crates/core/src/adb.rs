use std::io;
use std::path::PathBuf;
use std::process::Command;

/// Component launched on the tablet once the tunnel is up.
const APP_COMPONENT: &str = "dev.tablink.display/.ui.StreamActivity";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdbState {
    Device,
    Unauthorized,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDevice {
    pub serial: String,
    pub state: AdbState,
}

/// Parses `adb devices` output. Lines that are not `<serial>\t<state>` (the
/// header, daemon start-up chatter, blanks) are ignored, as are states this
/// program cannot act on.
pub fn parse_devices(output: &str) -> Vec<AdbDevice> {
    output
        .lines()
        .filter_map(|line| {
            let (serial, state) = line.trim().split_once('\t')?;
            let state = match state.trim() {
                "device" => AdbState::Device,
                "unauthorized" => AdbState::Unauthorized,
                "offline" => AdbState::Offline,
                _ => return None,
            };
            Some(AdbDevice { serial: serial.trim().to_string(), state })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    NoDevice,
    /// A tablet is attached but the USB debugging prompt has not been accepted.
    Unauthorized,
    /// Serials of every ready device; the user must unplug all but one.
    Multiple(Vec<String>),
}

pub fn select_device(devs: &[AdbDevice]) -> Result<&AdbDevice, SelectError> {
    let ready: Vec<&AdbDevice> = devs.iter().filter(|d| d.state == AdbState::Device).collect();
    match ready.as_slice() {
        [one] => Ok(one),
        [] if devs.iter().any(|d| d.state == AdbState::Unauthorized) => Err(SelectError::Unauthorized),
        [] => Err(SelectError::NoDevice),
        many => Err(SelectError::Multiple(many.iter().map(|d| d.serial.clone()).collect())),
    }
}

pub struct Adb {
    pub exe: PathBuf,
}

impl Adb {
    /// Prefers an `adb` shipped next to this executable, else relies on PATH.
    pub fn locate() -> Adb {
        let name = if cfg!(windows) { "adb.exe" } else { "adb" };
        let bundled = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.join(name)));
        match bundled {
            Some(p) if p.is_file() => Adb { exe: p },
            _ => Adb { exe: PathBuf::from(name) },
        }
    }

    fn run(&self, args: &[&str]) -> io::Result<String> {
        let out = Command::new(&self.exe).args(args).output()?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(io::Error::other(format!("adb {}: {msg}", args.join(" "))));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn devices(&self) -> io::Result<Vec<AdbDevice>> {
        Ok(parse_devices(&self.run(&["devices"])?))
    }

    /// Replaces any stale forward on `port` left by an earlier run.
    pub fn forward(&self, serial: &str, port: u16) -> io::Result<()> {
        let _ = self.remove_forward(serial, port);
        let spec = format!("tcp:{port}");
        self.run(&["-s", serial, "forward", &spec, &spec]).map(drop)
    }

    pub fn remove_forward(&self, serial: &str, port: u16) -> io::Result<()> {
        self.run(&["-s", serial, "forward", "--remove", &format!("tcp:{port}")]).map(drop)
    }

    pub fn start_app(&self, serial: &str) -> io::Result<()> {
        self.run(&["-s", serial, "shell", "am", "start", "-n", APP_COMPONENT]).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(serial: &str, state: AdbState) -> AdbDevice {
        AdbDevice { serial: serial.into(), state }
    }

    #[test]
    fn parse_devices_typical() {
        let out = "List of devices attached\nR5CX123\tdevice\nABC\tunauthorized\n\n";
        assert_eq!(
            parse_devices(out),
            vec![dev("R5CX123", AdbState::Device), dev("ABC", AdbState::Unauthorized)]
        );
    }

    #[test]
    fn parse_devices_ignores_daemon_noise() {
        let out = "* daemon not running; starting now at tcp:5037\r\n* daemon started successfully\r\nList of devices attached\r\nR5CX123\tdevice\r\n";
        assert_eq!(parse_devices(out), vec![dev("R5CX123", AdbState::Device)]);
    }

    #[test]
    fn parse_devices_reads_offline() {
        assert_eq!(parse_devices("List of devices attached\nX\toffline\n"), vec![dev("X", AdbState::Offline)]);
    }

    #[test]
    fn select_single() {
        let devs = [dev("A", AdbState::Offline), dev("B", AdbState::Device)];
        assert_eq!(select_device(&devs).map(|d| d.serial.as_str()), Ok("B"));
    }

    #[test]
    fn select_unauthorized_only() {
        let devs = [dev("A", AdbState::Unauthorized)];
        assert_eq!(select_device(&devs), Err(SelectError::Unauthorized));
    }

    #[test]
    fn select_multiple() {
        let devs = [dev("A", AdbState::Device), dev("B", AdbState::Device)];
        assert_eq!(select_device(&devs), Err(SelectError::Multiple(vec!["A".into(), "B".into()])));
    }

    #[test]
    fn select_none() {
        assert_eq!(select_device(&[]), Err(SelectError::NoDevice));
        assert_eq!(select_device(&[dev("A", AdbState::Offline)]), Err(SelectError::NoDevice));
    }
}
