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
/// Seen on real hardware when the display topology changes (extend <->
/// duplicate): the duplication object is no longer usable.
const DXGI_ERROR_INVALID_CALL: i32 = 0x887A0001u32 as i32;

pub fn classify(hr: i32) -> Classified {
    match hr {
        DXGI_ERROR_WAIT_TIMEOUT => Classified::Timeout,
        DXGI_ERROR_ACCESS_LOST | DXGI_ERROR_INVALID_CALL => Classified::ModeChanged,
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
    use crate::cursor::{self, CursorKind, CursorShape};
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        IDXGIAdapter, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
        DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    };

    /// Edge of the scratch surface used to blend the pointer; larger cursors
    /// are cut off at this size.
    const CURSOR_SCRATCH: u32 = 256;

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

    /// A desktop image in BGRA with the mouse pointer drawn in. The texture is
    /// the duplicator's own and is overwritten by the next captured frame.
    pub struct CapturedFrame {
        pub texture: ID3D11Texture2D,
        pub pts_us: u64,
        pub width: u32,
        pub height: u32,
    }

    pub enum CaptureEvent {
        Frame(CapturedFrame),
        Timeout,
        /// Carries the HRESULT that reported the change, for the log.
        ModeChanged(i32),
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
        scratch: Option<ID3D11Texture2D>,
        pointer_shape: Option<CursorShape>,
        pointer_visible: bool,
        pointer_pos: (i32, i32),
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
                Ok(DesktopDuplicator {
                    device,
                    context,
                    duplication,
                    holding_frame: false,
                    last: None,
                    scratch: None,
                    pointer_shape: None,
                    pointer_visible: false,
                    pointer_pos: (0, 0),
                })
            }
        }

        pub fn device(&self) -> &ID3D11Device {
            &self.device
        }

        /// Resolution and refresh rate Windows runs the captured monitor at.
        pub fn mode(&self) -> (u32, u32, f64) {
            let desc: DXGI_OUTDUPL_DESC = unsafe { self.duplication.GetDesc() };
            let rate = desc.ModeDesc.RefreshRate;
            let hz = if rate.Denominator == 0 { 0.0 } else { rate.Numerator as f64 / rate.Denominator as f64 };
            (desc.ModeDesc.Width, desc.ModeDesc.Height, hz)
        }

        /// Waits up to `timeout_ms` for the desktop image or the mouse
        /// pointer to change.
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
                        Classified::ModeChanged => Ok(CaptureEvent::ModeChanged(e.code().0)),
                        Classified::Fatal => Err(err("AcquireNextFrame", e)),
                    };
                }
                self.holding_frame = true;

                let pointer_moved = info.LastMouseUpdateTime != 0;
                if pointer_moved {
                    self.pointer_visible = info.PointerPosition.Visible.as_bool();
                    self.pointer_pos = (info.PointerPosition.Position.x, info.PointerPosition.Position.y);
                }
                if info.PointerShapeBufferSize > 0 {
                    self.read_pointer_shape(info.PointerShapeBufferSize);
                }
                if info.LastPresentTime == 0 && !pointer_moved {
                    return Ok(CaptureEvent::Timeout);
                }

                // Every acquired frame carries the whole desktop, also when
                // only the pointer moved, so the copy starts out cursor-free.
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
                if let Some(copy) = self.last.as_ref().map(|(copy, _, _)| copy.clone()) {
                    let copy = &copy;
                    self.context.CopyResource(copy, &texture);
                    if desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM {
                        // A pointer that cannot be drawn is not worth losing the frame over.
                        let _ = self.draw_pointer(&texture, copy, desc.Width, desc.Height);
                    }
                }
                Ok(self.last_frame().map_or(CaptureEvent::Timeout, CaptureEvent::Frame))
            }
        }

        /// The most recent desktop image again, stamped with the current time.
        pub fn last_frame(&self) -> Option<CapturedFrame> {
            let (texture, width, height) = self.last.as_ref()?;
            Some(CapturedFrame { texture: texture.clone(), pts_us: now_us(), width: *width, height: *height })
        }

        unsafe fn read_pointer_shape(&mut self, size: u32) {
            let mut data = vec![0u8; size as usize];
            let mut required = 0u32;
            let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            if self.duplication.GetFramePointerShape(size, data.as_mut_ptr() as *mut _, &mut required, &mut info).is_err() {
                return;
            }
            let Some(kind) = CursorKind::from_dxgi(info.Type) else { return };
            let height = if kind == CursorKind::Monochrome { info.Height / 2 } else { info.Height };
            let shape = CursorShape { kind, width: info.Width, height, pitch: info.Pitch, data };
            if shape.is_valid() {
                self.pointer_shape = Some(shape);
            }
        }

        /// Blends the pointer into `target`. The pixels under the pointer are
        /// read back from `source` through a small staging texture, blended on
        /// the CPU and written back; the frame itself never leaves the GPU.
        unsafe fn draw_pointer(
            &mut self,
            source: &ID3D11Texture2D,
            target: &ID3D11Texture2D,
            width: u32,
            height: u32,
        ) -> Result<(), CaptureError> {
            if !self.pointer_visible {
                return Ok(());
            }
            let Some(shape) = &self.pointer_shape else { return Ok(()) };
            let (px, py) = self.pointer_pos;
            let Some(clip) = cursor::clip(px, py, shape.width, shape.height, width, height, CURSOR_SCRATCH) else {
                return Ok(());
            };

            if self.scratch.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: CURSOR_SCRATCH,
                    Height: CURSOR_SCRATCH,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut scratch = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut scratch)).map_err(|e| err("cursor scratch", e))?;
                self.scratch = scratch;
            }
            let Some(scratch) = &self.scratch else { return Ok(()) };

            let region = D3D11_BOX {
                left: clip.x,
                top: clip.y,
                front: 0,
                right: clip.x + clip.width,
                bottom: clip.y + clip.height,
                back: 1,
            };
            self.context.CopySubresourceRegion(scratch, 0, 0, 0, 0, source, 0, Some(&region));

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(scratch, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| err("map cursor scratch", e))?;
            let stride = clip.width as usize * 4;
            let mut pixels = vec![0u8; stride * clip.height as usize];
            for row in 0..clip.height as usize {
                let src = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src, pixels.as_mut_ptr().add(row * stride), stride);
            }
            self.context.Unmap(scratch, 0);

            cursor::blend(&mut pixels, stride, clip.width, clip.height, shape, clip.src_x, clip.src_y);
            self.context.UpdateSubresource(target, 0, Some(&region), pixels.as_ptr() as *const _, stride as u32, 0);
            Ok(())
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
    fn invalid_call_maps_to_mode_changed() {
        assert_eq!(classify(0x887A0001u32 as i32), Classified::ModeChanged);
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
