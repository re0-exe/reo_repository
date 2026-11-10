//! Hardware HEVC encoding through Media Foundation.
//!
//! The encoder is looked up for the adapter that owns the capture device, so
//! the BGRA desktop texture is converted to NV12 and encoded without leaving
//! that GPU. Hardware encoders are asynchronous transforms: input may only be
//! submitted after a `METransformNeedInput` event, and output is collected
//! after `METransformHaveOutput`.

use super::{contains_irap, split_parameter_sets, EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::capture::CapturedFrame;
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use windows::core::{Interface, VARIANT};
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

/// NV12 textures cycled as encoder input. The encoder may still be reading a
/// texture when the next frame arrives, so they are never reused back to back.
const INPUT_RING: usize = 8;
/// How long the encoder is given to ask for input before it is considered stuck.
const INPUT_WAIT: Duration = Duration::from_millis(500);
/// Effectively "never" at 120 fps; keyframes are sent on request instead.
const GOP_FRAMES: u32 = 100_000;

// BT.709, studio range output; full range RGB input. Bit layout of
// D3D11_VIDEO_PROCESSOR_COLOR_SPACE: Usage:1, RGB_Range:1, YCbCr_Matrix:1,
// YCbCr_xvYCC:1, Nominal_Range:2.
const COLOR_SPACE_NV12_BT709_LIMITED: u32 = (1 << 2) | (1 << 4);
const COLOR_SPACE_RGB_FULL: u32 = 0;

fn mf(what: &str) -> impl Fn(windows::core::Error) -> EncodeError + '_ {
    move |e| EncodeError::Mf(format!("{what}: {e}"))
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub struct MfHevcEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    _device_manager: IMFDXGIDeviceManager,

    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    processor_enum: ID3D11VideoProcessorEnumerator,
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    ring_next: usize,

    cfg: EncoderConfig,
    need_input: u32,
    /// Frames submitted whose output has not been collected yet. Each one
    /// still owns a ring texture.
    in_encoder: usize,
    ready: VecDeque<EncodedFrame>,
    parameter_sets: Vec<u8>,
}

// The COM objects are only ever used from the thread that owns the encoder.
unsafe impl Send for MfHevcEncoder {}

impl MfHevcEncoder {
    pub fn new(device: &ID3D11Device, cfg: &EncoderConfig) -> Result<Self, EncodeError> {
        unsafe {
            // Already-initialized COM on this thread is fine.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(mf("MFStartup"))?;

            let transform = find_hardware_encoder(device)?;
            let attributes = transform.GetAttributes().map_err(mf("encoder attributes"))?;
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(mf("async unlock"))?;

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(mf("device manager"))?;
            let manager: IMFDXGIDeviceManager = manager.ok_or(EncodeError::Mf("no device manager".into()))?;
            manager.ResetDevice(device, token).map_err(mf("ResetDevice"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(mf("set D3D manager"))?;

            let codec_api: ICodecAPI = transform.cast().map_err(mf("ICodecAPI"))?;
            // Not every vendor accepts every property; the ones that matter
            // for correctness are set through the media types below.
            let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            let _ = codec_api
                .SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
            let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(cfg.bitrate_kbps * 1000));
            let _ = codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
            let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(GOP_FRAMES));

            // The output type must be set before the input type.
            let output = MFCreateMediaType().map_err(mf("output type"))?;
            set_common_video_attributes(&output, cfg)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC).map_err(mf("output subtype"))?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps * 1000).map_err(mf("bitrate"))?;
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)
                .map_err(mf("profile"))?;
            transform.SetOutputType(0, &output, 0).map_err(mf("SetOutputType"))?;

            let input = MFCreateMediaType().map_err(mf("input type"))?;
            set_common_video_attributes(&input, cfg)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(mf("input subtype"))?;
            transform.SetInputType(0, &input, 0).map_err(mf("SetInputType"))?;

            let events: IMFMediaEventGenerator = transform.cast().map_err(mf("event generator"))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(mf("begin streaming"))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(mf("start of stream"))?;

            let video_device: ID3D11VideoDevice = device.cast().map_err(mf("ID3D11VideoDevice"))?;
            let context = device.GetImmediateContext().map_err(mf("immediate context"))?;
            let video_context: ID3D11VideoContext = context.cast().map_err(mf("ID3D11VideoContext"))?;

            let rate = DXGI_RATIONAL { Numerator: cfg.fps as u32, Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: cfg.width,
                InputHeight: cfg.height,
                OutputFrameRate: rate,
                OutputWidth: cfg.width,
                OutputHeight: cfg.height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let processor_enum =
                video_device.CreateVideoProcessorEnumerator(&content).map_err(mf("video processor enumerator"))?;
            let processor = video_device.CreateVideoProcessor(&processor_enum, 0).map_err(mf("video processor"))?;

            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: COLOR_SPACE_RGB_FULL },
            );
            video_context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: COLOR_SPACE_NV12_BT709_LIMITED },
            );
            let full = RECT { left: 0, top: 0, right: cfg.width as i32, bottom: cfg.height as i32 };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&full));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));

            let mut ring = Vec::with_capacity(INPUT_RING);
            for _ in 0..INPUT_RING {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: cfg.width,
                    Height: cfg.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut texture = None;
                device.CreateTexture2D(&desc, None, Some(&mut texture)).map_err(mf("NV12 texture"))?;
                let texture: ID3D11Texture2D = texture.ok_or(EncodeError::Mf("no NV12 texture".into()))?;

                let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                };
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(&texture, &processor_enum, &view_desc, Some(&mut view))
                    .map_err(mf("output view"))?;
                ring.push((texture, view.ok_or(EncodeError::Mf("no output view".into()))?));
            }

            Ok(MfHevcEncoder {
                transform,
                events,
                codec_api,
                _device_manager: manager,
                video_device,
                video_context,
                processor,
                processor_enum,
                ring,
                ring_next: 0,
                cfg: cfg.clone(),
                need_input: 0,
                in_encoder: 0,
                ready: VecDeque::new(),
                parameter_sets: Vec::new(),
            })
        }
    }

    /// Whether this encoder was configured for frames of this size.
    pub fn matches(&self, width: u32, height: u32) -> bool {
        self.cfg.width == width && self.cfg.height == height
    }

    /// Converts the BGRA desktop texture into the next NV12 ring texture.
    unsafe fn convert(&mut self, frame: &CapturedFrame) -> Result<ID3D11Texture2D, EncodeError> {
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut input_view = None;
        self.video_device
            .CreateVideoProcessorInputView(&frame.texture, &self.processor_enum, &input_desc, Some(&mut input_view))
            .map_err(mf("input view"))?;

        let (texture, output_view) = &self.ring[self.ring_next];
        self.ring_next = (self.ring_next + 1) % self.ring.len();

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            pInputSurface: ManuallyDrop::new(input_view),
            ..Default::default()
        };
        let result = self.video_context.VideoProcessorBlt(&self.processor, output_view, 0, std::slice::from_ref(&stream));
        ManuallyDrop::drop(&mut stream.pInputSurface);
        result.map_err(mf("VideoProcessorBlt"))?;
        Ok(texture.clone())
    }

    /// Handles every event that is already queued. Returns whether anything
    /// was processed.
    unsafe fn pump_events(&mut self) -> Result<bool, EncodeError> {
        let mut any = false;
        loop {
            let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(any),
                Err(e) => return Err(mf("GetEvent")(e)),
            };
            any = true;
            let ty = event.GetType().map_err(mf("event type"))?;
            if ty == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if ty == METransformHaveOutput.0 as u32 {
                self.collect_output()?;
            }
        }
    }

    unsafe fn collect_output(&mut self) -> Result<(), EncodeError> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
        let mut status = 0u32;
        let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
        ManuallyDrop::drop(&mut buffers[0].pEvents);

        if let Err(e) = result {
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                // The encoder refined its output type; accept its first offer.
                let ty = self.transform.GetOutputAvailableType(0, 0).map_err(mf("output type change"))?;
                self.transform.SetOutputType(0, &ty, 0).map_err(mf("SetOutputType after change"))?;
                return Ok(());
            }
            return Err(mf("ProcessOutput")(e));
        }
        let Some(sample) = sample else { return Ok(()) };

        let buffer = sample.ConvertToContiguousBuffer().map_err(mf("output buffer"))?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len)).map_err(mf("lock output"))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();

        self.in_encoder = self.in_encoder.saturating_sub(1);
        // Not every vendor sets the clean-point attribute; the bitstream is
        // the authority on whether a decoder can start here.
        let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 || contains_irap(&data);
        let pts_us = (sample.GetSampleTime().unwrap_or(0) / 10).max(0) as u64;
        if keyframe {
            let (ps, _) = split_parameter_sets(&data);
            if !ps.is_empty() {
                self.parameter_sets = ps;
            }
        }
        self.ready.push_back(EncodedFrame { data, keyframe, pts_us });
        Ok(())
    }

    /// Polls the event queue until `done` holds or `timeout` passes.
    unsafe fn pump_until(&mut self, timeout: Duration, done: impl Fn(&Self) -> bool) -> Result<bool, EncodeError> {
        let deadline = Instant::now() + timeout;
        loop {
            let progressed = self.pump_events()?;
            if done(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            if !progressed {
                std::thread::sleep(Duration::from_micros(250));
            }
        }
    }
}

impl VideoEncoder for MfHevcEncoder {
    fn parameter_sets(&self) -> Vec<u8> {
        self.parameter_sets.clone()
    }

    fn encode(&mut self, frame: &CapturedFrame) -> Result<Option<EncodedFrame>, EncodeError> {
        if frame.width != self.cfg.width || frame.height != self.cfg.height {
            return Err(EncodeError::Mf(format!(
                "frame is {}x{}, encoder was configured for {}x{}",
                frame.width, frame.height, self.cfg.width, self.cfg.height
            )));
        }
        unsafe {
            // Never hand the encoder a ring texture it may still be reading.
            if !self.pump_until(INPUT_WAIT, |s| s.need_input > 0 && s.in_encoder < INPUT_RING - 1)? {
                return Err(EncodeError::Mf("encoder stopped requesting input".into()));
            }
            let texture = self.convert(frame)?;
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false).map_err(mf("surface buffer"))?;
            let sample = MFCreateSample().map_err(mf("sample"))?;
            sample.AddBuffer(&buffer).map_err(mf("AddBuffer"))?;
            sample.SetSampleTime(frame.pts_us as i64 * 10).map_err(mf("sample time"))?;
            sample.SetSampleDuration(10_000_000 / self.cfg.fps.max(1) as i64).map_err(mf("sample duration"))?;

            self.transform.ProcessInput(0, &sample, 0).map_err(mf("ProcessInput"))?;
            self.need_input -= 1;
            self.in_encoder += 1;

            // Wait at most half a frame interval for this frame to come back.
            // An encoder that answers one frame late then costs no throughput:
            // its output is picked up by the next call or by `poll`.
            let wait = Duration::from_micros(500_000 / self.cfg.fps.max(1) as u64);
            self.pump_until(wait, |s| !s.ready.is_empty())?;
        }
        Ok(self.ready.pop_front())
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>, EncodeError> {
        unsafe {
            self.pump_events()?;
        }
        Ok(self.ready.pop_front())
    }

    fn request_keyframe(&mut self) {
        unsafe {
            let _ = self.codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
        }
    }
}

impl Drop for MfHevcEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            // An asynchronous transform keeps its event queue and the hardware
            // encoder session until it is shut down explicitly. A new encoder
            // is created on every reconnect, so leaking them would add up.
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

unsafe fn set_common_video_attributes(ty: &IMFMediaType, cfg: &EncoderConfig) -> Result<(), EncodeError> {
    ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(mf("major type"))?;
    ty.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height)).map_err(mf("frame size"))?;
    ty.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps as u32, 1)).map_err(mf("frame rate"))?;
    ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(mf("aspect ratio"))?;
    ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(mf("interlace mode"))?;
    ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32).map_err(mf("yuv matrix"))?;
    ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32).map_err(mf("nominal range"))?;
    Ok(())
}

fn wide(w: &[u16]) -> String {
    let len = w.iter().position(|c| *c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..len])
}

unsafe fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = windows::core::PWSTR::null();
    let mut len = 0u32;
    if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_err() || name.is_null() {
        return "?".into();
    }
    let text = name.to_string().unwrap_or_else(|_| "?".into());
    CoTaskMemFree(Some(name.0 as *const _));
    text
}

/// Hardware HEVC encoders, optionally only those of one adapter.
unsafe fn enumerate_encoders(adapter_luid: Option<&[u8; 8]>) -> Result<Vec<IMFActivate>, EncodeError> {
    let mut filter = None;
    MFCreateAttributes(&mut filter, 1).map_err(mf("enum attributes"))?;
    let filter: IMFAttributes = filter.ok_or(EncodeError::Mf("no enum attributes".into()))?;
    if let Some(luid) = adapter_luid {
        filter.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid).map_err(mf("adapter filter"))?;
    }

    let output_type = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_HEVC };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnum2(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        None,
        Some(&output_type),
        &filter,
        &mut activates,
        &mut count,
    )
    .map_err(mf("MFTEnum2"))?;

    let mut found = Vec::new();
    if !activates.is_null() {
        for i in 0..count as usize {
            if let Some(activate) = (*activates.add(i)).take() {
                found.push(activate);
            }
        }
        CoTaskMemFree(Some(activates as *const _));
    }
    Ok(found)
}

/// Finds the hardware HEVC encoder that belongs to `device`'s adapter. On a
/// machine with two GPUs each vendor registers its own encoder, and an encoder
/// only works with textures from its own adapter.
unsafe fn find_hardware_encoder(device: &ID3D11Device) -> Result<IMFTransform, EncodeError> {
    let dxgi: IDXGIDevice = device.cast().map_err(mf("IDXGIDevice"))?;
    let desc = dxgi.GetAdapter().and_then(|a| a.GetDesc()).map_err(mf("adapter"))?;
    let adapter = wide(&desc.Description);
    let mut luid = [0u8; 8];
    luid[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
    luid[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());

    let mut tried = Vec::new();
    for activate in enumerate_encoders(Some(&luid))? {
        let name = friendly_name(&activate);
        match activate.ActivateObject::<IMFTransform>() {
            Ok(transform) => {
                println!("Encoder: {name} ({adapter})");
                return Ok(transform);
            }
            Err(e) => tried.push(format!("{name}: {e}")),
        }
    }

    let all: Vec<String> = enumerate_encoders(None)?.iter().map(|a| friendly_name(a)).collect();
    Err(EncodeError::NoHardwareEncoder(format!(
        "capture GPU '{adapter}'; encoders for it: {}; all hardware HEVC encoders on this PC: {}",
        if tried.is_empty() { "none".to_string() } else { tried.join("; ") },
        if all.is_empty() { "none".to_string() } else { all.join(", ") },
    )))
}
