//! Video encoding. The pipeline talks to [`VideoEncoder`]; the first
//! implementation is the Media Foundation hardware HEVC encoder.

#[cfg(windows)]
mod mf_hevc;
#[cfg(windows)]
pub use mf_hevc::MfHevcEncoder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub bitrate_kbps: u32,
}

/// One access unit in Annex-B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: u64,
}

#[derive(Debug)]
pub enum EncodeError {
    /// No hardware HEVC encoder could be started for the capture GPU; the
    /// text says which GPU and what was tried. There is no software fallback:
    /// it could not meet the latency target.
    NoHardwareEncoder(String),
    Mf(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::NoHardwareEncoder(detail) => write!(f, "no hardware HEVC encoder available: {detail}"),
            EncodeError::Mf(m) => write!(f, "encoder error: {m}"),
        }
    }
}
impl std::error::Error for EncodeError {}

#[cfg(windows)]
pub trait VideoEncoder {
    /// VPS+SPS+PPS in Annex-B; empty until the encoder has produced them.
    fn parameter_sets(&self) -> Vec<u8>;
    /// Submits a frame. Returns an encoded frame when one is ready, which
    /// with a low-latency encoder is normally the frame just submitted.
    fn encode(&mut self, frame: &crate::capture::CapturedFrame) -> Result<Option<EncodedFrame>, EncodeError>;
    /// Collects a frame the encoder finished after `encode` had returned.
    fn poll(&mut self) -> Result<Option<EncodedFrame>, EncodeError>;
    fn request_keyframe(&mut self);
}

const BITS_PER_PIXEL: f64 = 0.14;
const MIN_BITRATE_KBPS: u32 = 20_000;
const MAX_BITRATE_KBPS: u32 = 150_000;

/// Bitrate for visually lossless desktop content, bounded to what the USB
/// tunnel is expected to carry.
pub fn default_bitrate_kbps(width: u32, height: u32, fps: u16) -> u32 {
    let kbps = width as f64 * height as f64 * fps as f64 * BITS_PER_PIXEL / 1000.0;
    (kbps as u32).clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)
}

const NAL_VPS: u8 = 32;
const NAL_PPS: u8 = 34;

/// Splits an Annex-B HEVC stream into its parameter sets (VPS/SPS/PPS) and
/// everything else. Both halves are re-emitted with 4-byte start codes.
pub fn split_parameter_sets(annexb: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut ps, mut rest) = (Vec::new(), Vec::new());
    for nal in nal_units(annexb) {
        let Some(first) = nal.first() else { continue };
        let ty = (first >> 1) & 0x3F;
        let dst = if (NAL_VPS..=NAL_PPS).contains(&ty) { &mut ps } else { &mut rest };
        dst.extend_from_slice(&[0, 0, 0, 1]);
        dst.extend_from_slice(nal);
    }
    (ps, rest)
}

/// Whether the stream holds an IRAP picture (NAL types 16-21), i.e. a frame a
/// decoder can start from. Used alongside the encoder's own keyframe flag,
/// which not every vendor sets.
pub fn contains_irap(annexb: &[u8]) -> bool {
    nal_units(annexb).iter().any(|nal| nal.first().is_some_and(|b| (16..=21).contains(&((b >> 1) & 0x3F))))
}

/// NAL unit payloads (without start codes) of an Annex-B stream.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    // Positions just past each `00 00 01`.
    let starts: Vec<usize> = (0..data.len().saturating_sub(2))
        .filter(|&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
        .map(|i| i + 3)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &begin)| {
            // The next unit's start code is `00 00 01`, optionally preceded
            // by the extra zero of a 4-byte code; neither belongs to this unit.
            let mut end = starts.get(n + 1).map_or(data.len(), |next| next - 3);
            if starts.get(n + 1).is_some() && end > begin && data[end - 1] == 0 {
                end -= 1;
            }
            &data[begin..end]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_for_tab_s10_plus_120hz() {
        assert_eq!(default_bitrate_kbps(2800, 1752, 120), 82_414);
    }

    #[test]
    fn bitrate_clamped_low() {
        assert_eq!(default_bitrate_kbps(640, 480, 30), 20_000);
    }

    #[test]
    fn bitrate_clamped_high() {
        assert_eq!(default_bitrate_kbps(7680, 4320, 120), 150_000);
    }

    // HEVC NAL header byte 0 is `type << 1`.
    const VPS: u8 = 32 << 1;
    const SPS: u8 = 33 << 1;
    const PPS: u8 = 34 << 1;
    const IDR: u8 = 19 << 1;

    #[test]
    fn split_parameter_sets_separates_vps_sps_pps() {
        let stream = [0, 0, 0, 1, VPS, 1, 0xAA, 0, 0, 0, 1, SPS, 1, 0xBB, 0, 0, 0, 1, PPS, 1, 0xCC, 0, 0, 0, 1, IDR, 1, 0xDD, 0xEE];
        let (ps, rest) = split_parameter_sets(&stream);
        assert_eq!(ps, [0, 0, 0, 1, VPS, 1, 0xAA, 0, 0, 0, 1, SPS, 1, 0xBB, 0, 0, 0, 1, PPS, 1, 0xCC]);
        assert_eq!(rest, [0, 0, 0, 1, IDR, 1, 0xDD, 0xEE]);
    }

    #[test]
    fn split_handles_3_and_4_byte_start_codes() {
        let stream = [0, 0, 1, SPS, 1, 0xBB, 0, 0, 0, 1, IDR, 1, 0xDD, 0, 0, 1, PPS, 1, 0xCC];
        let (ps, rest) = split_parameter_sets(&stream);
        assert_eq!(ps, [0, 0, 0, 1, SPS, 1, 0xBB, 0, 0, 0, 1, PPS, 1, 0xCC]);
        assert_eq!(rest, [0, 0, 0, 1, IDR, 1, 0xDD]);
    }

    #[test]
    fn irap_is_detected_from_nal_types() {
        assert!(contains_irap(&[0, 0, 0, 1, VPS, 1, 0, 0, 1, IDR, 1, 0xDD]));
        assert!(contains_irap(&[0, 0, 1, 21 << 1, 1, 0xDD])); // CRA
        assert!(!contains_irap(&[0, 0, 0, 1, 1 << 1, 1, 0xDD])); // trailing picture
        assert!(!contains_irap(&[0, 0, 0, 1, VPS, 1, 0xAA]));
        assert!(!contains_irap(&[]));
    }

    #[test]
    fn split_of_stream_without_start_code_is_empty() {
        assert_eq!(split_parameter_sets(&[1, 2, 3]), (vec![], vec![]));
        assert_eq!(split_parameter_sets(&[]), (vec![], vec![]));
    }
}
