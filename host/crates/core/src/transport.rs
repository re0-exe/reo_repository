use protocol::Channel;
use std::io::{self, Write};
use std::net::{Ipv4Addr, TcpStream};

/// A way to open the per-channel byte streams to the tablet. Kept abstract so
/// that another USB transport (e.g. AOA) can be added without touching the
/// pipeline.
pub trait Transport {
    fn open(&self, ch: Channel) -> io::Result<TcpStream>;
}

/// TCP over an `adb forward` tunnel: connects to the local end of the forward
/// and announces the channel with its first byte.
pub struct AdbTransport {
    pub port: u16,
}

impl Transport for AdbTransport {
    fn open(&self, ch: Channel) -> io::Result<TcpStream> {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port))?;
        s.set_nodelay(true)?;
        s.write_all(&[ch as u8])?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn transport_sends_channel_byte() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = AdbTransport { port }.open(Channel::Video).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut b = [0u8; 1];
        server.read_exact(&mut b).unwrap();
        assert_eq!(b[0], 2);
        assert!(client.nodelay().unwrap());
    }

    #[test]
    fn transport_reports_refused_connection() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(AdbTransport { port }.open(Channel::Control).is_err());
    }
}
