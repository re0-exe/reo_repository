use crate::{HEADER_LEN, MAX_PAYLOAD};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Hello = 0x01,
    Config = 0x02,
    Pointer = 0x03,
    RequestKeyframe = 0x04,
    Ping = 0x05,
    Pong = 0x06,
    AudioRoute = 0x07,
    FrameAck = 0x08,
    VideoConfig = 0x10,
    VideoFrame = 0x11,
    AudioFrame = 0x20,
}

impl MsgType {
    fn from_u8(b: u8) -> Option<MsgType> {
        use MsgType::*;
        Some(match b {
            0x01 => Hello,
            0x02 => Config,
            0x03 => Pointer,
            0x04 => RequestKeyframe,
            0x05 => Ping,
            0x06 => Pong,
            0x07 => AudioRoute,
            0x08 => FrameAck,
            0x10 => VideoConfig,
            0x11 => VideoFrame,
            0x20 => AudioFrame,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnknownType(u8),
    PayloadTooLarge(u32),
    Truncated,
    BadVersion(u16),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::UnknownType(t) => write!(f, "unknown message type 0x{t:02x}"),
            ProtocolError::PayloadTooLarge(n) => write!(f, "payload of {n} bytes exceeds limit"),
            ProtocolError::Truncated => write!(f, "message payload is truncated"),
            ProtocolError::BadVersion(v) => write!(f, "unsupported protocol version {v}"),
        }
    }
}
impl std::error::Error for ProtocolError {}

/// `type u8 | flags u8 | length u32 | pts_us u64`, little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub ty: MsgType,
    pub flags: u8,
    pub length: u32,
    pub pts_us: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = self.ty as u8;
        b[1] = self.flags;
        b[2..6].copy_from_slice(&self.length.to_le_bytes());
        b[6..14].copy_from_slice(&self.pts_us.to_le_bytes());
        b
    }

    pub fn decode(buf: &[u8; HEADER_LEN]) -> Result<Header, ProtocolError> {
        let ty = MsgType::from_u8(buf[0]).ok_or(ProtocolError::UnknownType(buf[0]))?;
        let length = u32::from_le_bytes(buf[2..6].try_into().unwrap());
        if length > MAX_PAYLOAD {
            return Err(ProtocolError::PayloadTooLarge(length));
        }
        let pts_us = u64::from_le_bytes(buf[6..14].try_into().unwrap());
        Ok(Header { ty, flags: buf[1], length, pts_us })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FLAG_KEYFRAME, MAX_PAYLOAD};

    #[test]
    fn header_roundtrip() {
        let h = Header { ty: MsgType::VideoFrame, flags: FLAG_KEYFRAME, length: 5, pts_us: 0x0102030405060708 };
        let bytes = h.encode();
        assert_eq!(bytes, [0x11, 0x01, 5, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(Header::decode(&bytes), Ok(h));
    }

    #[test]
    fn header_rejects_unknown_type() {
        let mut bytes = [0u8; 14];
        bytes[0] = 0xFF;
        assert_eq!(Header::decode(&bytes), Err(ProtocolError::UnknownType(0xFF)));
    }

    #[test]
    fn header_rejects_oversize() {
        let h = Header { ty: MsgType::VideoFrame, flags: 0, length: MAX_PAYLOAD + 1, pts_us: 0 };
        assert_eq!(Header::decode(&h.encode()), Err(ProtocolError::PayloadTooLarge(MAX_PAYLOAD + 1)));
    }
}
