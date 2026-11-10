//! Integration with the open-source Virtual Display Driver (IddCx). See
//! `docs/driver.md` for what was verified about it and what was not.

/// Where the driver reads its settings from.
pub const DRIVER_CONFIG_PATH: &str = r"C:\VirtualDisplayDriver\vdd_settings.xml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: Vec<u16>,
}

#[derive(Debug)]
pub enum DisplayError {
    /// The driver's settings file or device was not found.
    DriverMissing,
    /// Writing the settings or toggling the device failed (usually: not elevated).
    EnableFailed(String),
    /// The driver is enabled but Windows exposes no output for it. Carries a
    /// report of what Windows does expose.
    OutputNotFound(String),
}

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayError::DriverMissing => write!(f, "virtual display driver is not installed"),
            DisplayError::EnableFailed(m) => write!(f, "could not enable the virtual display: {m}"),
            DisplayError::OutputNotFound(report) => write!(f, "virtual display output not found{report}"),
        }
    }
}
impl std::error::Error for DisplayError {}

/// Rewrites the driver's settings so that it exposes exactly one monitor with
/// `mode`, leaving every section this program does not own untouched (the
/// driver's file carries logging, EDID and colour sections whose defaults
/// must survive).
pub fn patch_driver_config(existing: &str, mode: &DisplayMode, gpu: Option<&str>) -> String {
    let rates: String =
        mode.refresh_hz.iter().map(|hz| format!("\n        <g_refresh_rate>{hz}</g_refresh_rate>")).collect();
    let resolutions: String = mode
        .refresh_hz
        .iter()
        .map(|hz| {
            format!(
                "\n        <resolution>\n            <width>{}</width>\n            <height>{}</height>\n            <refresh_rate>{hz}</refresh_rate>\n        </resolution>",
                mode.width, mode.height
            )
        })
        .collect();

    let mut out = replace_block(existing, "monitors", "\n        <count>1</count>\n    ");
    out = replace_block(&out, "global", &format!("{rates}\n    "));
    out = replace_block(&out, "resolutions", &format!("{resolutions}\n    "));
    if let Some(name) = gpu {
        out = replace_block(&out, "gpu", &format!("\n        <friendlyname>{}</friendlyname>\n    ", xml_escape(name)));
    }
    out
}

fn replace_block(xml: &str, tag: &str, inner: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let block = format!("{open}{inner}{close}");
    if let (Some(start), Some(end)) = (xml.find(&open), xml.find(&close)) {
        if start < end {
            return format!("{}{block}{}", &xml[..start], &xml[end + close.len()..]);
        }
    }
    match xml.rfind("</vdd_settings>") {
        Some(root_end) => format!("{}    {block}\n{}", &xml[..root_end], &xml[root_end..]),
        None => format!("{xml}{block}\n"),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The virtual display should render on the discrete GPU so that capture and
/// hardware encode share one device. Returns `None` to keep the driver's own
/// choice when no NVIDIA adapter is present.
pub fn preferred_gpu(adapter_names: &[String]) -> Option<&str> {
    adapter_names.iter().map(String::as_str).find(|n| n.to_ascii_uppercase().contains("NVIDIA"))
}

/// True for the display device the driver creates, by description or id.
pub fn is_virtual_display(description: &str, device_id: &str) -> bool {
    let d = description.to_ascii_lowercase();
    let id = device_id.to_ascii_lowercase();
    d.contains("virtual display") || d.contains("iddsampledriver") || id.contains("mttvdd")
}

/// Extracts the driver's instance id from `pnputil /enum-devices` output.
/// Field labels are localized by Windows, so this relies only on layout: each
/// device is a paragraph whose first line carries the instance id.
pub fn find_driver_instance(pnputil_output: &str) -> Option<String> {
    let normalized = pnputil_output.replace("\r\n", "\n");
    normalized.split("\n\n").find_map(|block| {
        let value = |line: &str| line.split_once(':').map(|(_, v)| v.trim().to_string());
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let id = value(lines.next()?)?;
        let matches = lines.filter_map(value).any(|v| is_virtual_display(&v, ""));
        (matches && id.contains('\\')).then_some(id)
    })
}

/// One GDI display device (`\\\\.\\DISPLAYn`) with the ids of the monitors on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayDevice {
    pub name: String,
    pub description: String,
    pub device_id: String,
    pub monitor_ids: Vec<String>,
    /// Part of the Windows desktop.
    pub attached: bool,
}

/// Display devices that belong to the virtual display driver. A device
/// qualifies by its own name or id, by the id of a monitor on it, or by not
/// having existed in `before`, a snapshot taken while the driver was disabled.
/// The last rule makes detection independent of how the driver names itself;
/// an empty snapshot disables it.
pub fn virtual_candidates<'a>(before: &[String], now: &'a [DisplayDevice]) -> Vec<&'a DisplayDevice> {
    now.iter()
        .filter(|d| {
            is_virtual_display(&d.description, &d.device_id)
                || d.monitor_ids.iter().any(|m| is_virtual_display("", m) || m.to_ascii_uppercase().contains("MTT1337"))
                || (!before.is_empty() && !before.contains(&d.name))
        })
        .collect()
}

#[cfg(windows)]
pub use win::VirtualDisplay;

#[cfg(windows)]
mod win {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Devices::Display::{SetDisplayConfig, SDC_APPLY, SDC_TOPOLOGY_EXTEND};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1};
    use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP};

    /// How long Windows is given to bring the monitor up after the driver is
    /// (re)started.
    const OUTPUT_WAIT: Duration = Duration::from_secs(20);
    /// How long to wait for the monitor to join the desktop on its own before
    /// asking Windows to extend onto it.
    const EXTEND_AFTER: Duration = Duration::from_secs(3);

    /// The enabled virtual monitor. Dropping it disables the driver's device,
    /// which removes the monitor from Windows.
    pub struct VirtualDisplay {
        instance_id: String,
        /// Display device names that existed while the driver was disabled.
        before: Vec<String>,
    }

    fn wide_to_string(w: &[u16]) -> String {
        let len = w.iter().position(|c| *c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..len])
    }

    fn pnputil(args: &[&str]) -> Result<String, DisplayError> {
        let out = Command::new("pnputil")
            .args(args)
            .output()
            .map_err(|e| DisplayError::EnableFailed(format!("pnputil: {e}")))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            return Err(DisplayError::EnableFailed(format!("pnputil {}: {}", args.join(" "), text.trim())));
        }
        Ok(text)
    }

    fn driver_instance() -> Option<String> {
        find_driver_instance(&pnputil(&["/enum-devices", "/class", "Display"]).ok()?)
    }

    fn adapter_names() -> Vec<String> {
        let mut names = Vec::new();
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return names };
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                if let Ok(desc) = adapter.GetDesc1() {
                    names.push(wide_to_string(&desc.Description));
                }
                i += 1;
            }
        }
        names
    }

    fn enum_device(parent: Option<&str>, index: u32) -> Option<DISPLAY_DEVICEW> {
        let mut dd = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
        let wide: Option<Vec<u16>> = parent.map(|p| p.encode_utf16().chain(std::iter::once(0)).collect());
        let name = wide.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
        unsafe { EnumDisplayDevicesW(name, index, &mut dd, 0) }.as_bool().then_some(dd)
    }

    fn display_devices() -> Vec<DisplayDevice> {
        let mut devices = Vec::new();
        let mut i = 0;
        while let Some(dd) = enum_device(None, i) {
            i += 1;
            let name = wide_to_string(&dd.DeviceName);
            let mut monitor_ids = Vec::new();
            let mut m = 0;
            while let Some(monitor) = enum_device(Some(&name), m) {
                m += 1;
                monitor_ids.push(wide_to_string(&monitor.DeviceID));
            }
            devices.push(DisplayDevice {
                name,
                description: wide_to_string(&dd.DeviceString),
                device_id: wide_to_string(&dd.DeviceID),
                monitor_ids,
                attached: dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0,
            });
        }
        devices
    }

    fn dxgi_outputs() -> Vec<(String, String, IDXGIOutput1)> {
        let mut outputs = Vec::new();
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return outputs };
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                a += 1;
                let adapter_name = adapter.GetDesc1().map(|d| wide_to_string(&d.Description)).unwrap_or_default();
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    o += 1;
                    let (Ok(desc), Ok(output1)) = (output.GetDesc(), output.cast::<IDXGIOutput1>()) else { continue };
                    outputs.push((adapter_name.clone(), wide_to_string(&desc.DeviceName), output1));
                }
            }
        }
        outputs
    }

    /// What Windows currently reports, for the error message when the
    /// virtual monitor cannot be found.
    fn diagnostics(before: &[String]) -> String {
        let mut text = String::from("\n  Display devices:");
        for d in display_devices() {
            text.push_str(&format!(
                "\n    {} | {} | {} | attached={} | new={} | monitors={:?}",
                d.name,
                d.description,
                d.device_id,
                d.attached,
                !before.contains(&d.name),
                d.monitor_ids
            ));
        }
        text.push_str("\n  DXGI outputs:");
        for (adapter, name, _) in dxgi_outputs() {
            text.push_str(&format!("\n    {name} on {adapter}"));
        }
        text
    }

    impl VirtualDisplay {
        pub fn is_driver_installed() -> bool {
            std::path::Path::new(DRIVER_CONFIG_PATH).is_file() && driver_instance().is_some()
        }

        /// Removes a monitor left behind by a run that was killed before it
        /// could clean up.
        pub fn disable_leftover() {
            if let Some(id) = driver_instance() {
                let _ = pnputil(&["/disable-device", &id]);
            }
        }

        /// Points the driver at `mode` and brings its monitor up. Requires an
        /// elevated process: toggling a device is an administrative action.
        pub fn enable(mode: &DisplayMode) -> Result<VirtualDisplay, DisplayError> {
            let existing = std::fs::read_to_string(DRIVER_CONFIG_PATH).map_err(|_| DisplayError::DriverMissing)?;
            let instance_id = driver_instance().ok_or(DisplayError::DriverMissing)?;

            let adapters = adapter_names();
            let patched = patch_driver_config(&existing, mode, preferred_gpu(&adapters));
            if patched != existing {
                std::fs::write(DRIVER_CONFIG_PATH, &patched)
                    .map_err(|e| DisplayError::EnableFailed(format!("{DRIVER_CONFIG_PATH}: {e}")))?;
            }

            // Start from a disabled device: the driver reads its settings when
            // it starts, and the snapshot taken here is what later tells the
            // virtual monitor apart from the real ones.
            pnputil(&["/disable-device", &instance_id])?;
            let before: Vec<String> = display_devices().into_iter().map(|d| d.name).collect();
            pnputil(&["/enable-device", &instance_id])?;

            let display = VirtualDisplay { instance_id, before };
            let started = Instant::now();
            let mut extended = false;
            loop {
                if display.find_output().is_some() {
                    return Ok(display);
                }
                // The monitor exists but Windows did not add it to the desktop
                // (for example when projection is set to "PC screen only").
                if !extended && started.elapsed() >= EXTEND_AFTER && display.detached_candidate() {
                    extended = true;
                    unsafe {
                        let _ = SetDisplayConfig(None, None, SDC_TOPOLOGY_EXTEND | SDC_APPLY);
                    }
                }
                if started.elapsed() >= OUTPUT_WAIT {
                    let report = diagnostics(&display.before);
                    return Err(DisplayError::OutputNotFound(report));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }

        fn detached_candidate(&self) -> bool {
            let devices = display_devices();
            let candidates = virtual_candidates(&self.before, &devices);
            !candidates.is_empty() && candidates.iter().all(|d| !d.attached)
        }

        fn find(&self) -> Option<(String, String, IDXGIOutput1)> {
            let devices = display_devices();
            let names: Vec<&str> = virtual_candidates(&self.before, &devices)
                .into_iter()
                .filter(|d| d.attached)
                .map(|d| d.name.as_str())
                .collect();
            if names.is_empty() {
                return None;
            }
            dxgi_outputs().into_iter().find(|(_, name, _)| names.contains(&name.as_str()))
        }

        fn find_output(&self) -> Option<IDXGIOutput1> {
            self.find().map(|(_, _, output)| output)
        }

        /// Which output is being captured and on which GPU, for the log.
        pub fn describe(&self) -> String {
            match self.find() {
                Some((adapter, name, _)) => format!("{name} ({adapter})"),
                None => "sanal monitör bulunamadı".into(),
            }
        }

        /// The DXGI output of the virtual monitor. Looked up on every call
        /// because a mode change invalidates earlier handles.
        pub fn output(&self) -> Result<IDXGIOutput1, DisplayError> {
            self.find_output().ok_or_else(|| DisplayError::OutputNotFound(String::new()))
        }
    }

    impl Drop for VirtualDisplay {
        fn drop(&mut self) {
            let _ = pnputil(&["/disable-device", &self.instance_id]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<?xml version='1.0' encoding='utf-8'?>
<vdd_settings>
    <monitors>
        <count>2</count>
    </monitors>
    <gpu>
        <friendlyname>default</friendlyname>
    </gpu>
    <global>
        <g_refresh_rate>60</g_refresh_rate>
        <g_refresh_rate>144</g_refresh_rate>
    </global>
    <resolutions>
        <resolution>
            <width>1920</width>
            <height>1080</height>
            <refresh_rate>60</refresh_rate>
        </resolution>
    </resolutions>
    <colour>
        <SDR10bit>false</SDR10bit>
        <HDRPlus>false</HDRPlus>
    </colour>
</vdd_settings>
";

    fn mode() -> DisplayMode {
        DisplayMode { width: 2800, height: 1752, refresh_hz: vec![60, 120] }
    }

    #[test]
    fn driver_config_contains_mode() {
        let out = patch_driver_config(SAMPLE, &mode(), None);
        assert!(out.contains("<count>1</count>"));
        assert!(out.contains("<g_refresh_rate>60</g_refresh_rate>"));
        assert!(out.contains("<g_refresh_rate>120</g_refresh_rate>"));
        assert!(out.contains("<width>2800</width>"));
        assert!(out.contains("<height>1752</height>"));
        assert!(out.contains("<refresh_rate>120</refresh_rate>"));
    }

    #[test]
    fn driver_config_drops_previous_modes() {
        let out = patch_driver_config(SAMPLE, &mode(), None);
        assert!(!out.contains("<width>1920</width>"));
        assert!(!out.contains("<g_refresh_rate>144</g_refresh_rate>"));
        assert!(!out.contains("<count>2</count>"));
    }

    #[test]
    fn driver_config_keeps_unrelated_sections() {
        let out = patch_driver_config(SAMPLE, &mode(), None);
        assert!(out.contains("<HDRPlus>false</HDRPlus>"));
        assert!(out.contains("<friendlyname>default</friendlyname>"));
        assert!(out.trim_end().ends_with("</vdd_settings>"));
    }

    #[test]
    fn driver_config_sets_gpu_when_given() {
        let out = patch_driver_config(SAMPLE, &mode(), Some("NVIDIA GeForce RTX 3050 Laptop GPU"));
        assert!(out.contains("<friendlyname>NVIDIA GeForce RTX 3050 Laptop GPU</friendlyname>"));
        assert!(!out.contains("<friendlyname>default</friendlyname>"));
    }

    #[test]
    fn driver_config_adds_missing_section() {
        let minimal = "<vdd_settings>\n</vdd_settings>\n";
        let out = patch_driver_config(minimal, &mode(), None);
        assert!(out.contains("<width>2800</width>"));
        assert!(out.contains("<count>1</count>"));
        assert!(out.trim_end().ends_with("</vdd_settings>"));
    }

    #[test]
    fn driver_config_is_idempotent() {
        let once = patch_driver_config(SAMPLE, &mode(), None);
        assert_eq!(patch_driver_config(&once, &mode(), None), once);
    }

    #[test]
    fn prefers_nvidia_adapter() {
        let names = ["AMD Radeon(TM) Graphics".to_string(), "NVIDIA GeForce RTX 3050 Laptop GPU".to_string()];
        assert_eq!(preferred_gpu(&names), Some("NVIDIA GeForce RTX 3050 Laptop GPU"));
        assert_eq!(preferred_gpu(&names[..1]), None);
    }

    #[test]
    fn finds_driver_instance_in_pnputil_output() {
        let out = "Microsoft PnP Utility\r\n\r\nInstance ID:                PCI\\VEN_10DE&DEV_25A2\\4&1\r\nDevice Description:         NVIDIA GeForce RTX 3050 Laptop GPU\r\nClass Name:                 Display\r\n\r\nInstance ID:                ROOT\\DISPLAY\\0000\r\nDevice Description:         Virtual Display Driver\r\nClass Name:                 Display\r\nStatus:                     Started\r\n\r\n";
        assert_eq!(find_driver_instance(out).as_deref(), Some("ROOT\\DISPLAY\\0000"));
    }

    #[test]
    fn finds_driver_instance_with_localized_labels() {
        let out = "Örnek Kimliği:              ROOT\\DISPLAY\\0001\r\nCihaz Açıklaması:           Virtual Display Driver\r\n";
        assert_eq!(find_driver_instance(out).as_deref(), Some("ROOT\\DISPLAY\\0001"));
    }

    #[test]
    fn no_driver_instance_when_absent() {
        let out = "Instance ID:                PCI\\VEN_10DE\\4&1\r\nDevice Description:         NVIDIA GeForce RTX 3050 Laptop GPU\r\n";
        assert_eq!(find_driver_instance(out), None);
    }

    fn device(name: &str, description: &str, monitor: &str, attached: bool) -> DisplayDevice {
        DisplayDevice {
            name: name.into(),
            description: description.into(),
            device_id: String::new(),
            monitor_ids: vec![monitor.into()],
            attached,
        }
    }

    #[test]
    fn candidate_by_adapter_description() {
        let now = [device("D1", "AMD Radeon(TM) Graphics", "MONITOR\\NCP004D", true), device("D5", "Virtual Display Driver", "", true)];
        let names: Vec<&str> = virtual_candidates(&[], &now).iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["D5"]);
    }

    #[test]
    fn candidate_by_monitor_id() {
        let now = [device("D1", "AMD Radeon(TM) Graphics", "MONITOR\\NCP004D\\x", true), device("D7", "Some Adapter", "MONITOR\\MTT1337\\{guid}", true)];
        let names: Vec<&str> = virtual_candidates(&[], &now).iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["D7"]);
    }

    #[test]
    fn candidate_by_being_new_since_snapshot() {
        let before = ["D1".to_string(), "D2".to_string()];
        let now = [device("D1", "AMD", "", true), device("D2", "AMD", "", false), device("D9", "Unknown Adapter", "", true)];
        let names: Vec<&str> = virtual_candidates(&before, &now).iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["D9"]);
    }

    #[test]
    fn empty_snapshot_does_not_make_everything_a_candidate() {
        let now = [device("D1", "AMD", "", true)];
        assert!(virtual_candidates(&[], &now).is_empty());
    }

    #[test]
    fn recognises_virtual_display_device() {
        assert!(is_virtual_display("Virtual Display Driver", "Root\\MttVDD"));
        assert!(is_virtual_display("IddSampleDriver Device", "ROOT\\MTTVDD"));
        assert!(!is_virtual_display("NVIDIA GeForce RTX 3050 Laptop GPU", "PCI\\VEN_10DE"));
    }
}
