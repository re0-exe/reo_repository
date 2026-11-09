//! Desktop Duplication capture of the virtual monitor. Frames stay on the GPU.

/// What a failed `AcquireNextFrame` means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classified {
    /// Nothing new was presented within the timeout.
    Timeout,
    /// The output's mode, orientation or desktop changed; duplication must be
    /// created again.
    ModeChanged,
    Fatal,
}

const DXGI_ERROR_ACCESS_LOST: i32 = 0x887A0026u32 as i32;
const DXGI_ERROR_WAIT_TIMEOUT: i32 = 0x887A0027u32 as i32;

pub fn classify(hr: i32) -> Classified {
    match hr {
        DXGI_ERROR_WAIT_TIMEOUT => Classified::Timeout,
        DXGI_ERROR_ACCESS_LOST => Classified::ModeChanged,
        _ => Classified::Fatal,
    }
}

/// Microseconds on a process-wide monotonic clock, so that timestamps stay
/// ordered across duplicator re-creation.
pub fn now_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(windows)]
pub use win::{CaptureError, CaptureEvent, CapturedFrame, DesktopDuplicator};

#[cfg(windows)]
mod win {
    use super::*;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIAdapter, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_OUTDUPL_FRAME_INFO,
    };

    #[derive(Debug)]
    pub struct CaptureError(pub String);

    impl std::fmt::Display for CaptureError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "screen capture failed: {}", self.0)
        }
    }
    impl std::error::Error for CaptureError {}

    fn err(what: &str, e: windows::core::Error) -> CaptureError {
        CaptureError(format!("{what}: {e}"))
    }

    /// A desktop image in BGRA. The texture is the duplicator's own copy of
    /// the desktop and is overwritten by the next captured frame.
    pub struct CapturedFrame {
        pub texture: ID3D11Texture2D,
        pub pts_us: u64,
        pub width: u32,
        pub height: u32,
    }

    pub enum CaptureEvent {
        Frame(CapturedFrame),
        Timeout,
        ModeChanged,
    }

    pub struct DesktopDuplicator {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
        holding_frame: bool,
        /// Copy of the most recent desktop image. Duplication only reports
        /// changes, so a frame that was skipped under backpressure would
        /// otherwise be lost for as long as the screen stays still.
        last: Option<(ID3D11Texture2D, u32, u32)>,
    }

    impl DesktopDuplicator {
        /// Creates a D3D11 device on the adapter that owns `output`;
        /// duplication only works from that adapter.
        pub fn new(output: &IDXGIOutput1) -> Result<Self, CaptureError> {
            unsafe {
                let adapter: IDXGIAdapter = output.GetParent().map_err(|e| err("output adapter", e))?;
                let mut device = None;
                let mut context = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .map_err(|e| err("D3D11CreateDevice", e))?;
                let device: ID3D11Device = device.ok_or_else(|| CaptureError("no D3D11 device".into()))?;
                let context: ID3D11DeviceContext = context.ok_or_else(|| CaptureError("no D3D11 context".into()))?;

                // The encoder uses this device from Media Foundation's threads.
                let mt: ID3D11Multithread = context.cast().map_err(|e| err("ID3D11Multithread", e))?;
                let _ = mt.SetMultithreadProtected(true);

                let duplication = output.DuplicateOutput(&device).map_err(|e| err("DuplicateOutput", e))?;
                Ok(DesktopDuplicator { device, context, duplication, holding_frame: false, last: None })
            }
        }

        pub fn device(&self) -> &ID3D11Device {
            &self.device
        }

        /// Waits up to `timeout_ms` for the desktop to change. Pointer-only
        /// updates are reported as `Timeout`: the image itself is unchanged.
        pub fn next(&mut self, timeout_ms: u32) -> Result<CaptureEvent, CaptureError> {
            unsafe {
                if self.holding_frame {
                    let _ = self.duplication.ReleaseFrame();
                    self.holding_frame = false;
                }
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                if let Err(e) = self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                    return match classify(e.code().0) {
                        Classified::Timeout => Ok(CaptureEvent::Timeout),
                        Classified::ModeChanged => Ok(CaptureEvent::ModeChanged),
                        Classified::Fatal => Err(err("AcquireNextFrame", e)),
                    };
                }
                self.holding_frame = true;
                if info.LastPresentTime == 0 {
                    return Ok(CaptureEvent::Timeout);
                }
                let resource = resource.ok_or_else(|| CaptureError("frame without a surface".into()))?;
                let texture: ID3D11Texture2D = resource.cast().map_err(|e| err("frame texture", e))?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);

                let reusable = matches!(&self.last, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
                if !reusable {
                    let copy_desc = D3D11_TEXTURE2D_DESC {
                        MipLevels: 1,
                        ArraySize: 1,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                        ..desc
                    };
                    let mut copy = None;
                    self.device.CreateTexture2D(&copy_desc, None, Some(&mut copy)).map_err(|e| err("frame copy", e))?;
                    let copy = copy.ok_or_else(|| CaptureError("no frame copy texture".into()))?;
                    self.last = Some((copy, desc.Width, desc.Height));
                }
                if let Some((copy, _, _)) = &self.last {
                    self.context.CopyResource(copy, &texture);
                }
                Ok(self.last_frame().map_or(CaptureEvent::Timeout, CaptureEvent::Frame))
            }
        }

        /// The most recent desktop image again, stamped with the current time.
        pub fn last_frame(&self) -> Option<CapturedFrame> {
            let (texture, width, height) = self.last.as_ref()?;
            Some(CapturedFrame { texture: texture.clone(), pts_us: now_us(), width: *width, height: *height })
        }
    }

    impl Drop for DesktopDuplicator {
        fn drop(&mut self) {
            if self.holding_frame {
                unsafe {
                    let _ = self.duplication.ReleaseFrame();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_lost_maps_to_mode_changed() {
        assert_eq!(classify(0x887A0026u32 as i32), Classified::ModeChanged);
    }

    #[test]
    fn wait_timeout_maps_to_timeout() {
        assert_eq!(classify(0x887A0027u32 as i32), Classified::Timeout);
    }

    #[test]
    fn other_error_is_fatal() {
        assert_eq!(classify(0x80004005u32 as i32), Classified::Fatal);
        assert_eq!(classify(0x887A0005u32 as i32), Classified::Fatal); // device removed
    }
}
