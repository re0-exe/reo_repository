//! HEVC encoding through NVIDIA's NVENC API, used directly.
//!
//! The Media Foundation wrapper NVIDIA registers with Windows does not start
//! on every machine, and it hides the latency controls. Talking to NVENC
//! itself gives the ultra-low-latency tuning, takes the BGRA desktop texture
//! as input without a separate colour conversion pass, and returns each frame
//! from the same call that submitted it.
//!
//! The driver library is loaded at run time, so the program still starts on
//! machines without an NVIDIA GPU.

use super::{contains_irap, split_parameter_sets, EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::capture::CapturedFrame;
use moq_nvenc::sys::nvEncodeAPI as nv;
use std::ffi::{c_void, CStr};
use std::sync::OnceLock;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

/// VBV size in frames. Small enough to keep the encoder from buffering,
/// large enough that a keyframe is not starved of bits.
const VBV_FRAMES: u32 = 2;

struct Api {
    fns: nv::NV_ENCODE_API_FUNCTION_LIST,
    _library: libloading::Library,
}
// The table holds plain function pointers into the loaded driver library.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let library = libloading::Library::new("nvEncodeAPI64.dll").map_err(|e| format!("nvEncodeAPI64.dll: {e}"))?;

        type GetMaxVersion = unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS;
        type CreateInstance = unsafe extern "C" fn(*mut nv::NV_ENCODE_API_FUNCTION_LIST) -> nv::NVENCSTATUS;
        let get_max: GetMaxVersion = *library
            .get::<GetMaxVersion>(b"NvEncodeAPIGetMaxSupportedVersion\0")
            .map_err(|e| format!("NvEncodeAPIGetMaxSupportedVersion: {e}"))?;
        let create: CreateInstance = *library
            .get::<CreateInstance>(b"NvEncodeAPICreateInstance\0")
            .map_err(|e| format!("NvEncodeAPICreateInstance: {e}"))?;

        let mut max = 0u32;
        let status = get_max(&mut max);
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPIGetMaxSupportedVersion failed: {status:?}"));
        }
        let wanted = (nv::NVENCAPI_MAJOR_VERSION << 4) | nv::NVENCAPI_MINOR_VERSION;
        if max < wanted {
            return Err(format!(
                "the NVIDIA driver supports NVENC {}.{}, but {}.{} is needed; update the graphics driver",
                max >> 4,
                max & 0xF,
                nv::NVENCAPI_MAJOR_VERSION,
                nv::NVENCAPI_MINOR_VERSION
            ));
        }

        let mut fns: nv::NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
        fns.version = nv::NV_ENCODE_API_FUNCTION_LIST_VER;
        let status = create(&mut fns);
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPICreateInstance failed: {status:?}"));
        }
        Ok(Api { fns, _library: library })
    })
    .as_ref()
    .map_err(String::clone)
}

fn missing(name: &str) -> EncodeError {
    EncodeError::Mf(format!("NVENC entry point {name} is missing"))
}

pub struct NvencEncoder {
    api: &'static Api,
    encoder: *mut c_void,
    bitstream: *mut c_void,
    /// The texture currently registered as input, with its NVENC handle.
    registered: Option<(ID3D11Texture2D, *mut c_void)>,
    cfg: EncoderConfig,
    force_keyframe: bool,
    parameter_sets: Vec<u8>,
    _device: ID3D11Device,
}

impl NvencEncoder {
    pub fn new(device: &ID3D11Device, cfg: &EncoderConfig) -> Result<Self, EncodeError> {
        let api = api().map_err(EncodeError::Mf)?;
        unsafe {
            let mut open: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            open.version = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            open.deviceType = nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = device.as_raw();
            open.apiVersion = nv::NVENCAPI_VERSION;

            let mut encoder: *mut c_void = std::ptr::null_mut();
            let open_session = api.fns.nvEncOpenEncodeSessionEx.ok_or_else(|| missing("nvEncOpenEncodeSessionEx"))?;
            let status = open_session(&mut open, &mut encoder);

            // From here on the session is owned by `this`, whose Drop closes it.
            let mut this = NvencEncoder {
                api,
                encoder,
                bitstream: std::ptr::null_mut(),
                registered: None,
                cfg: cfg.clone(),
                force_keyframe: true,
                parameter_sets: Vec::new(),
                _device: device.clone(),
            };
            this.check("open session", status)?;

            let bitrate = cfg.bitrate_kbps.saturating_mul(1000);
            let fps = cfg.fps.max(1) as u32;

            let mut preset: nv::NV_ENC_PRESET_CONFIG = std::mem::zeroed();
            preset.version = nv::NV_ENC_PRESET_CONFIG_VER;
            preset.presetCfg.version = nv::NV_ENC_CONFIG_VER;
            let get_preset =
                api.fns.nvEncGetEncodePresetConfigEx.ok_or_else(|| missing("nvEncGetEncodePresetConfigEx"))?;
            let status = get_preset(
                this.encoder,
                nv::NV_ENC_CODEC_HEVC_GUID,
                nv::NV_ENC_PRESET_P1_GUID,
                nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset,
            );
            this.check("preset", status)?;

            let mut config = preset.presetCfg;
            config.version = nv::NV_ENC_CONFIG_VER;
            config.profileGUID = nv::NV_ENC_HEVC_PROFILE_MAIN_GUID;
            // No periodic keyframes: they are sent when the tablet asks.
            config.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
            config.frameIntervalP = 1;
            config.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            config.rcParams.averageBitRate = bitrate;
            config.rcParams.maxBitRate = bitrate;
            config.rcParams.vbvBufferSize = bitrate / fps * VBV_FRAMES;
            config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
            config.rcParams.set_zeroReorderDelay(1);
            config.rcParams.set_enableLookahead(0);

            let hevc = &mut config.encodeCodecConfig.hevcConfig;
            hevc.idrPeriod = nv::NVENC_INFINITE_GOPLENGTH;
            hevc.set_repeatSPSPPS(1);
            hevc.set_chromaFormatIDC(1);
            // BT.709, studio range. NVENC converts the RGB input with this
            // matrix, and the tablet's decoder reads it back from the stream.
            let vui = &mut hevc.hevcVUIParameters;
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFormat = nv::NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            vui.videoFullRangeFlag = 0;
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            vui.transferCharacteristics =
                nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            vui.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
            // Tells the decoder that frames are never reordered, so it can
            // output each one as soon as it is decoded.
            vui.bitstreamRestrictionFlag = 1;

            let mut init: nv::NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
            init.version = nv::NV_ENC_INITIALIZE_PARAMS_VER;
            init.encodeGUID = nv::NV_ENC_CODEC_HEVC_GUID;
            init.presetGUID = nv::NV_ENC_PRESET_P1_GUID;
            init.tuningInfo = nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
            init.encodeWidth = cfg.width;
            init.encodeHeight = cfg.height;
            init.maxEncodeWidth = cfg.width;
            init.maxEncodeHeight = cfg.height;
            init.darWidth = cfg.width;
            init.darHeight = cfg.height;
            init.frameRateNum = fps;
            init.frameRateDen = 1;
            init.enableEncodeAsync = 0;
            init.enablePTD = 1;
            init.encodeConfig = &mut config;
            let initialize = api.fns.nvEncInitializeEncoder.ok_or_else(|| missing("nvEncInitializeEncoder"))?;
            let status = initialize(this.encoder, &mut init);
            this.check("initialize", status)?;

            let mut buffer: nv::NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
            buffer.version = nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            let create_bitstream =
                api.fns.nvEncCreateBitstreamBuffer.ok_or_else(|| missing("nvEncCreateBitstreamBuffer"))?;
            let status = create_bitstream(this.encoder, &mut buffer);
            this.check("bitstream buffer", status)?;
            this.bitstream = buffer.bitstreamBuffer;

            Ok(this)
        }
    }

    /// Turns an NVENC status into an error that carries the driver's own
    /// description of what went wrong.
    fn check(&self, what: &str, status: nv::NVENCSTATUS) -> Result<(), EncodeError> {
        if status == nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Ok(());
        }
        let mut detail = String::new();
        if !self.encoder.is_null() {
            if let Some(last_error) = self.api.fns.nvEncGetLastErrorString {
                let text = unsafe { last_error(self.encoder) };
                if !text.is_null() {
                    detail = unsafe { CStr::from_ptr(text) }.to_string_lossy().trim().to_string();
                }
            }
        }
        Err(EncodeError::Mf(format!("NVENC {what}: {status:?} {detail}")))
    }

    /// Registers `texture` as encoder input, replacing the previous one if
    /// the capture side switched to a new texture.
    unsafe fn register(&mut self, texture: &ID3D11Texture2D) -> Result<*mut c_void, EncodeError> {
        if let Some((current, handle)) = &self.registered {
            if current.as_raw() == texture.as_raw() {
                return Ok(*handle);
            }
        }
        self.unregister();

        let mut params: nv::NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
        params.version = nv::NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType = nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        params.width = self.cfg.width;
        params.height = self.cfg.height;
        params.resourceToRegister = texture.as_raw();
        // DXGI's B8G8R8A8 is what NVENC calls ARGB.
        params.bufferFormat = nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        params.bufferUsage = nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
        let register = self.api.fns.nvEncRegisterResource.ok_or_else(|| missing("nvEncRegisterResource"))?;
        let status = register(self.encoder, &mut params);
        self.check("register input", status)?;
        self.registered = Some((texture.clone(), params.registeredResource));
        Ok(params.registeredResource)
    }

    unsafe fn unregister(&mut self) {
        if let Some((_, handle)) = self.registered.take() {
            if let Some(unregister) = self.api.fns.nvEncUnregisterResource {
                let _ = unregister(self.encoder, handle);
            }
        }
    }
}

impl VideoEncoder for NvencEncoder {
    fn parameter_sets(&self) -> Vec<u8> {
        self.parameter_sets.clone()
    }

    fn matches(&self, width: u32, height: u32) -> bool {
        self.cfg.width == width && self.cfg.height == height
    }

    fn encode(&mut self, frame: &CapturedFrame) -> Result<Option<EncodedFrame>, EncodeError> {
        if !self.matches(frame.width, frame.height) {
            return Err(EncodeError::Mf(format!(
                "frame is {}x{}, encoder was configured for {}x{}",
                frame.width, frame.height, self.cfg.width, self.cfg.height
            )));
        }
        let fns = &self.api.fns;
        let map = fns.nvEncMapInputResource.ok_or_else(|| missing("nvEncMapInputResource"))?;
        let unmap = fns.nvEncUnmapInputResource.ok_or_else(|| missing("nvEncUnmapInputResource"))?;
        let encode = fns.nvEncEncodePicture.ok_or_else(|| missing("nvEncEncodePicture"))?;
        let lock = fns.nvEncLockBitstream.ok_or_else(|| missing("nvEncLockBitstream"))?;
        let unlock = fns.nvEncUnlockBitstream.ok_or_else(|| missing("nvEncUnlockBitstream"))?;

        unsafe {
            let registered = self.register(&frame.texture)?;

            let mut mapped: nv::NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            mapped.version = nv::NV_ENC_MAP_INPUT_RESOURCE_VER;
            mapped.registeredResource = registered;
            let status = map(self.encoder, &mut mapped);
            self.check("map input", status)?;

            let mut picture: nv::NV_ENC_PIC_PARAMS = std::mem::zeroed();
            picture.version = nv::NV_ENC_PIC_PARAMS_VER;
            picture.inputWidth = self.cfg.width;
            picture.inputHeight = self.cfg.height;
            picture.inputPitch = self.cfg.width;
            picture.inputBuffer = mapped.mappedResource;
            picture.bufferFmt = mapped.mappedBufferFmt;
            picture.outputBitstream = self.bitstream;
            picture.pictureStruct = nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            picture.inputTimeStamp = frame.pts_us;
            if self.force_keyframe {
                picture.encodePicFlags = nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                    | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32;
            }

            let status = encode(self.encoder, &mut picture);
            if status == nv::NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
                // Not expected without B-frames or lookahead, but harmless.
                let _ = unmap(self.encoder, mapped.mappedResource);
                return Ok(None);
            }
            if let Err(e) = self.check("encode", status) {
                let _ = unmap(self.encoder, mapped.mappedResource);
                return Err(e);
            }
            self.force_keyframe = false;

            // Blocks until the frame is finished; with this tuning that is a
            // few milliseconds.
            let mut locked: nv::NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
            locked.version = nv::NV_ENC_LOCK_BITSTREAM_VER;
            locked.outputBitstream = self.bitstream;
            let status = lock(self.encoder, &mut locked);
            if let Err(e) = self.check("lock bitstream", status) {
                let _ = unmap(self.encoder, mapped.mappedResource);
                return Err(e);
            }
            let data =
                std::slice::from_raw_parts(locked.bitstreamBufferPtr as *const u8, locked.bitstreamSizeInBytes as usize)
                    .to_vec();
            let _ = unlock(self.encoder, self.bitstream);
            let _ = unmap(self.encoder, mapped.mappedResource);

            let keyframe = contains_irap(&data);
            if keyframe {
                let (parameter_sets, _) = split_parameter_sets(&data);
                if !parameter_sets.is_empty() {
                    self.parameter_sets = parameter_sets;
                }
            }
            Ok(Some(EncodedFrame { data, keyframe, pts_us: frame.pts_us }))
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>, EncodeError> {
        // Every frame is returned by the `encode` call that submitted it.
        Ok(None)
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        unsafe {
            self.unregister();
            if !self.bitstream.is_null() {
                if let Some(destroy) = self.api.fns.nvEncDestroyBitstreamBuffer {
                    let _ = destroy(self.encoder, self.bitstream);
                }
            }
            if let Some(destroy) = self.api.fns.nvEncDestroyEncoder {
                let _ = destroy(self.encoder);
            }
        }
    }
}
