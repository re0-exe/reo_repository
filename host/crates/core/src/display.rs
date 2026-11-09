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
    /// The driver is enabled but Windows exposes no output for it.
    OutputNotFound,
}

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayError::DriverMissing => write!(f, "virtual display driver is not installed"),
            DisplayError::EnableFailed(m) => write!(f, "could not enable the virtual display: {m}"),
            DisplayError::OutputNotFound => write!(f, "virtual display output not found"),
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

#[cfg(windows)]
pub use win::VirtualDisplay;

#[cfg(windows)]
mod win {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1};
    use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP};

    /// How long Windows is given to bring the monitor up after the driver is
    /// (re)started.
    const OUTPUT_WAIT: Duration = Duration::from_secs(10);

    /// The enabled virtual monitor. Dropping it disables the driver's device,
    /// which removes the monitor from Windows.
    pub struct VirtualDisplay {
        instance_id: String,
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

    /// `\\.\DISPLAYn` names of the driver's displays that are part of the desktop.
    fn virtual_device_names() -> Vec<String> {
        let mut found = Vec::new();
        let mut i = 0;
        loop {
            let mut dd = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
                break;
            }
            i += 1;
            let attached = dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0;
            if attached && is_virtual_display(&wide_to_string(&dd.DeviceString), &wide_to_string(&dd.DeviceID)) {
                found.push(wide_to_string(&dd.DeviceName));
            }
        }
        found
    }

    fn find_output() -> Option<IDXGIOutput1> {
        let names = virtual_device_names();
        if names.is_empty() {
            return None;
        }
        unsafe {
            let factory = CreateDXGIFactory1::<IDXGIFactory1>().ok()?;
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                a += 1;
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    o += 1;
                    let Ok(desc) = output.GetDesc() else { continue };
                    if names.contains(&wide_to_string(&desc.DeviceName)) {
                        return output.cast::<IDXGIOutput1>().ok();
                    }
                }
            }
        }
        None
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
            let changed = patched != existing;
            if changed {
                std::fs::write(DRIVER_CONFIG_PATH, &patched)
                    .map_err(|e| DisplayError::EnableFailed(format!("{DRIVER_CONFIG_PATH}: {e}")))?;
            }

            // Enabling an already enabled device is a no-op; the restart is
            // what makes the driver re-read its settings.
            pnputil(&["/enable-device", &instance_id])?;
            if changed {
                pnputil(&["/restart-device", &instance_id])?;
            }

            let display = VirtualDisplay { instance_id };
            let deadline = Instant::now() + OUTPUT_WAIT;
            while find_output().is_none() {
                if Instant::now() >= deadline {
                    return Err(DisplayError::OutputNotFound);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Ok(display)
        }

        /// The DXGI output of the virtual monitor. Looked up on every call
        /// because a mode change invalidates earlier handles.
        pub fn output(&self) -> Result<IDXGIOutput1, DisplayError> {
            find_output().ok_or(DisplayError::OutputNotFound)
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

    #[test]
    fn recognises_virtual_display_device() {
        assert!(is_virtual_display("Virtual Display Driver", "Root\\MttVDD"));
        assert!(is_virtual_display("IddSampleDriver Device", "ROOT\\MTTVDD"));
        assert!(!is_virtual_display("NVIDIA GeForce RTX 3050 Laptop GPU", "PCI\\VEN_10DE"));
    }
}
