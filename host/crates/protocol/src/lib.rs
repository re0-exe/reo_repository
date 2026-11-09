//! TabLink Display wire format. Platform independent; shared with the Android
//! client through the golden vectors in `protocol-vectors/`.

mod header;
mod messages;

pub use header::{Header, MsgType, ProtocolError};
pub use messages::{read_message, write_message, Config, Hello, Message, ReadError};

pub const PROTOCOL_VERSION: u16 = 1;
pub const PORT: u16 = 27183;
pub const HEADER_LEN: usize = 14;
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
pub const FLAG_KEYFRAME: u8 = 0b1;

/// First byte written on every TCP connection, selecting its role.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Control = 1,
    Video = 2,
    Audio = 3,
}
