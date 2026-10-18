//! A replicated register: one leader, two followers. The leader acknowledges a
//! write once a majority has it, so a partitioned follower falls behind. The
//! planted bug lets every replica answer reads from its own copy.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

pub const PORT: u16 = 7002;

/// One request-reply line connection with a guest-time read timeout.
pub struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Connection {
    /// Connect and read with `timeout`: an unreachable peer fails fast
    /// instead of waiting out TCP's SYN retries.
    pub fn open(address: &str, timeout: Duration) -> std::io::Result<Self> {
        use std::net::ToSocketAddrs;
        let target = address
            .to_socket_addrs()?
            .next()
            .ok_or(std::io::ErrorKind::NotFound)?;
        let stream = TcpStream::connect_timeout(&target, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        })
    }
    pub fn request(&mut self, line: &str) -> std::io::Result<String> {
        self.writer.write_all(format!("{line}\n").as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(reply.trim().to_string())
    }
}
