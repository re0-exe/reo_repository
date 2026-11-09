use crate::{Header, MsgType, ProtocolError, FLAG_KEYFRAME, HEADER_LEN, MAX_PAYLOAD, PROTOCOL_VERSION};
use std::io::{self, Read, Write};

/// Sent by the tablet first. `caps` bits: 0 HEVC Main, 1 Main10, 2 4:4:4, 3 HDR10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: Vec<u16>,
    pub caps: u32,
    pub dpi: u16,
}

/// Host's answer. `codec`: 0 Main 4:2:0, 1 Main10, 2 4:4:4. `profile`: 0 Draw, 1 Video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub codec: u8,
    pub hdr: bool,
    pub profile: u8,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Config(Config),
    RequestKeyframe,
    Ping,
    Pong,
    /// VPS+SPS+PPS in Annex-B.
    VideoConfig(Vec<u8>),
    VideoFrame { keyframe: bool, data: Vec<u8> },
}

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    Protocol(ProtocolError),
}
impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}
impl From<ProtocolError> for ReadError {
    fn from(e: ProtocolError) -> Self {
        ReadError::Protocol(e)
    }
}
impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(e) => write!(f, "{e}"),
            ReadError::Protocol(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for ReadError {}

pub fn write_message<W: Write>(w: &mut W, pts_us: u64, m: &Message) -> io::Result<()> {
    let mut flags = 0u8;
    let mut owned = Vec::new();
    let (ty, payload): (MsgType, &[u8]) = match m {
        Message::Hello(h) => {
            owned.extend_from_slice(&h.version.to_le_bytes());
            owned.extend_from_slice(&h.width.to_le_bytes());
            owned.extend_from_slice(&h.height.to_le_bytes());
            let n = u8::try_from(h.refresh_hz.len())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many refresh rates"))?;
            owned.push(n);
            for hz in &h.refresh_hz {
                owned.extend_from_slice(&hz.to_le_bytes());
            }
            owned.extend_from_slice(&h.caps.to_le_bytes());
            owned.extend_from_slice(&h.dpi.to_le_bytes());
            (MsgType::Hello, &owned)
        }
        Message::Config(c) => {
            owned.extend_from_slice(&c.width.to_le_bytes());
            owned.extend_from_slice(&c.height.to_le_bytes());
            owned.extend_from_slice(&c.fps.to_le_bytes());
            owned.push(c.codec);
            owned.push(c.hdr as u8);
            owned.push(c.profile);
            owned.extend_from_slice(&c.bitrate_kbps.to_le_bytes());
            (MsgType::Config, &owned)
        }
        Message::RequestKeyframe => (MsgType::RequestKeyframe, &[]),
        Message::Ping => (MsgType::Ping, &[]),
        Message::Pong => (MsgType::Pong, &[]),
        Message::VideoConfig(d) => (MsgType::VideoConfig, d),
        Message::VideoFrame { keyframe, data } => {
            if *keyframe {
                flags |= FLAG_KEYFRAME;
            }
            (MsgType::VideoFrame, data)
        }
    };
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|n| *n <= MAX_PAYLOAD)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload exceeds MAX_PAYLOAD"))?;
    w.write_all(&Header { ty, flags, length, pts_us }.encode())?;
    w.write_all(payload)
}

/// Reads the next message this build understands. Message types reserved for
/// later phases are skipped. A stream that ends mid-message yields
/// `ReadError::Io` with kind `UnexpectedEof`.
pub fn read_message<R: Read>(r: &mut R) -> Result<(u64, Message), ReadError> {
    loop {
        let mut hb = [0u8; HEADER_LEN];
        r.read_exact(&mut hb)?;
        let h = Header::decode(&hb)?;
        let mut payload = vec![0u8; h.length as usize];
        r.read_exact(&mut payload)?;
        let msg = match h.ty {
            MsgType::Hello => Message::Hello(parse_hello(&payload)?),
            MsgType::Config => Message::Config(parse_config(&payload)?),
            MsgType::RequestKeyframe => Message::RequestKeyframe,
            MsgType::Ping => Message::Ping,
            MsgType::Pong => Message::Pong,
            MsgType::VideoConfig => Message::VideoConfig(payload),
            MsgType::VideoFrame => Message::VideoFrame { keyframe: h.flags & FLAG_KEYFRAME != 0, data: payload },
            MsgType::Pointer | MsgType::AudioRoute | MsgType::AudioFrame => continue,
        };
        return Ok((h.pts_us, msg));
    }
}

struct Fields<'a>(&'a [u8]);
impl Fields<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        if self.0.len() < N {
            return Err(ProtocolError::Truncated);
        }
        let (head, rest) = self.0.split_at(N);
        self.0 = rest;
        Ok(head.try_into().unwrap())
    }
    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
}

fn parse_hello(p: &[u8]) -> Result<Hello, ProtocolError> {
    let mut f = Fields(p);
    let version = f.u16()?;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::BadVersion(version));
    }
    let width = f.u32()?;
    let height = f.u32()?;
    let n = f.u8()?;
    let refresh_hz = (0..n).map(|_| f.u16()).collect::<Result<Vec<_>, _>>()?;
    Ok(Hello { version, width, height, refresh_hz, caps: f.u32()?, dpi: f.u16()? })
}

fn parse_config(p: &[u8]) -> Result<Config, ProtocolError> {
    let mut f = Fields(p);
    Ok(Config {
        width: f.u32()?,
        height: f.u32()?,
        fps: f.u16()?,
        codec: f.u8()?,
        hdr: f.u8()? != 0,
        profile: f.u8()?,
        bitrate_kbps: f.u32()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Header, MsgType, ProtocolError};
    use std::io::Cursor;
    use std::path::PathBuf;

    fn hello() -> Hello {
        Hello { version: 1, width: 2800, height: 1752, refresh_hz: vec![60, 120], caps: 0b0011, dpi: 266 }
    }
    fn config() -> Config {
        Config { width: 2800, height: 1752, fps: 120, codec: 0, hdr: false, profile: 0, bitrate_kbps: 80_000 }
    }
    fn bytes(pts: u64, m: &Message) -> Vec<u8> {
        let mut v = Vec::new();
        write_message(&mut v, pts, m).unwrap();
        v
    }
    fn roundtrip(m: Message) {
        let (pts, back) = read_message(&mut Cursor::new(bytes(7, &m))).unwrap();
        assert_eq!((pts, back), (7, m));
    }

    #[test]
    fn hello_roundtrip() {
        roundtrip(Message::Hello(hello()));
    }

    #[test]
    fn config_roundtrip() {
        roundtrip(Message::Config(config()));
    }

    #[test]
    fn hello_bad_version() {
        let m = Message::Hello(Hello { version: 2, ..hello() });
        match read_message(&mut Cursor::new(bytes(0, &m))) {
            Err(ReadError::Protocol(ProtocolError::BadVersion(2))) => {}
            other => panic!("expected BadVersion(2), got {other:?}"),
        }
    }

    #[test]
    fn read_truncated_stream() {
        let mut b = bytes(0, &Message::VideoFrame { keyframe: true, data: vec![1, 2, 3] });
        b.pop();
        match read_message(&mut Cursor::new(b)) {
            Err(ReadError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn read_rejects_oversize_before_reading_payload() {
        let h = Header { ty: MsgType::VideoFrame, flags: 0, length: crate::MAX_PAYLOAD + 1, pts_us: 0 };
        match read_message(&mut Cursor::new(h.encode().to_vec())) {
            Err(ReadError::Protocol(ProtocolError::PayloadTooLarge(_))) => {}
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn read_rejects_truncated_hello_payload() {
        let h = Header { ty: MsgType::Hello, flags: 0, length: 3, pts_us: 0 };
        let mut b = h.encode().to_vec();
        b.extend_from_slice(&[1, 0, 0]);
        match read_message(&mut Cursor::new(b)) {
            Err(ReadError::Protocol(ProtocolError::Truncated)) => {}
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn read_skips_unhandled_type() {
        let h = Header { ty: MsgType::Pointer, flags: 0, length: 3, pts_us: 0 };
        let mut b = h.encode().to_vec();
        b.extend_from_slice(&[9, 9, 9]);
        b.extend(bytes(0, &Message::Ping));
        assert_eq!(read_message(&mut Cursor::new(b)).unwrap().1, Message::Ping);
    }

    fn golden() -> Vec<(&'static str, u64, Message, &'static str)> {
        vec![
            ("hello.bin", 0, Message::Hello(hello()),
             r#"{"type":"Hello","version":1,"width":2800,"height":1752,"refreshHz":[60,120],"caps":3,"dpi":266}"#),
            ("config.bin", 0, Message::Config(config()),
             r#"{"type":"Config","width":2800,"height":1752,"fps":120,"codec":0,"hdr":false,"profile":0,"bitrateKbps":80000}"#),
            ("video_config.bin", 0, Message::VideoConfig(vec![0, 0, 0, 1, 0x40]),
             r#"{"type":"VideoConfig","data":[0,0,0,1,64]}"#),
            ("video_frame_key.bin", 1000, Message::VideoFrame { keyframe: true, data: vec![1, 2, 3] },
             r#"{"type":"VideoFrame","keyframe":true,"data":[1,2,3]}"#),
            ("ping.bin", 0, Message::Ping, r#"{"type":"Ping"}"#),
        ]
    }
    fn vectors_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../protocol-vectors")
    }

    #[test]
    fn golden_vectors_match() {
        for (name, pts, msg, _) in golden() {
            let expected = std::fs::read(vectors_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(bytes(pts, &msg), expected, "{name}");
        }
    }

    /// Run with `cargo test -p protocol regenerate_vectors -- --ignored` after
    /// an intentional wire format change.
    #[test]
    #[ignore]
    fn regenerate_vectors() {
        let mut json = String::from("[\n");
        let all = golden();
        for (i, (name, pts, msg, fields)) in all.iter().enumerate() {
            std::fs::write(vectors_dir().join(name), bytes(*pts, msg)).unwrap();
            let sep = if i + 1 == all.len() { "" } else { "," };
            json.push_str(&format!("  {{\"file\":\"{name}\",\"ptsUs\":{pts},\"message\":{fields}}}{sep}\n"));
        }
        json.push_str("]\n");
        std::fs::write(vectors_dir().join("vectors.json"), json).unwrap();
    }
}
