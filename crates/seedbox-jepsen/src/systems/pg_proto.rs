//! The PostgreSQL frontend/backend protocol, simple query only, with trust
//! authentication. There is no driver to pin.
use std::{
    io::{ErrorKind, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub enum PgError {
    /// An ErrorResponse: the server rejected the statement (SQLSTATE, message).
    Server { code: String, message: String },
    /// A timeout or a lost connection: the outcome is unknown.
    Indeterminate(String),
    /// The server wants a password; run it with trust authentication.
    Setup(String),
}

impl std::fmt::Display for PgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server { code, message } => write!(f, "{code}: {message}"),
            Self::Indeterminate(m) | Self::Setup(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, PgError>;
/// Text columns, `None` for NULL.
pub type Rows = Vec<Vec<Option<String>>>;

pub struct Pg {
    host: String,
    stream: TcpStream,
    timeout: Duration,
    buf: Vec<u8>,
}

fn fields(payload: &[u8]) -> (String, String) {
    let (mut code, mut message) = (String::new(), String::new());
    for item in payload.split(|&b| b == 0) {
        if let Some((&kind, rest)) = item.split_first() {
            let text = String::from_utf8_lossy(rest).to_string();
            match kind {
                b'C' => code = text,
                b'M' => message = text,
                _ => {}
            }
        }
    }
    (code, message)
}

impl Pg {
    pub fn connect(host: &str, port: u16, timeout: f64) -> Result<Self> {
        let timeout = Duration::from_secs_f64(timeout);
        let stream = crate::kit::http::connect(host, port, timeout)
            .map_err(|e| PgError::Indeterminate(e.0))?;
        let mut pg = Self {
            host: host.into(),
            stream,
            timeout,
            buf: Vec::new(),
        };
        let mut body = 196_608u32.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0postgres\0database\0postgres\0\0");
        let mut startup = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        startup.extend_from_slice(&body);
        pg.send(&startup)?;
        let deadline = Instant::now() + timeout;
        loop {
            let (kind, payload) = pg.message(deadline)?;
            match kind {
                b'R' if payload.get(..4) != Some(&[0, 0, 0, 0]) => {
                    return Err(PgError::Setup(
                        "server asked for a password; run postgres with trust authentication"
                            .into(),
                    ))
                }
                b'E' => {
                    let (code, message) = fields(&payload);
                    return Err(PgError::Server { code, message });
                }
                b'Z' => return Ok(pg),
                _ => {}
            }
        }
    }

    fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream
            .write_all(data)
            .map_err(|e| PgError::Indeterminate(format!("{}: send failed: {e}", self.host)))
    }

    fn message(&mut self, deadline: Instant) -> Result<(u8, Vec<u8>)> {
        loop {
            if self.buf.len() >= 5 {
                let length =
                    u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]])
                        as usize;
                if self.buf.len() > length {
                    let kind = self.buf[0];
                    let payload = self.buf[5..1 + length].to_vec();
                    self.buf.drain(..1 + length);
                    return Ok((kind, payload));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PgError::Indeterminate(format!("{}: timeout", self.host)));
            }
            let _ = self.stream.set_read_timeout(Some(deadline - now));
            let mut chunk = vec![0u8; 65_536];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(PgError::Indeterminate(format!(
                        "{}: server closed the connection",
                        self.host
                    )))
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Err(PgError::Indeterminate(format!("{}: timeout", self.host)))
                }
                Err(e) => return Err(PgError::Indeterminate(format!("{}: {e}", self.host))),
            }
        }
    }

    /// Run one statement: its rows (text columns, `None` for NULL) and command tag.
    pub fn query(&mut self, sql: &str) -> Result<(Rows, Option<String>)> {
        let mut frame = vec![b'Q'];
        frame.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
        frame.extend_from_slice(sql.as_bytes());
        frame.push(0);
        self.send(&frame)?;
        let deadline = Instant::now() + self.timeout;
        let (mut rows, mut tag, mut error) = (Vec::new(), None, None);
        loop {
            let (kind, payload) = self.message(deadline)?;
            match kind {
                b'D' => {
                    let count = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                    let (mut offset, mut row) = (2, Vec::with_capacity(count));
                    for _ in 0..count {
                        let size = i32::from_be_bytes(
                            payload[offset..offset + 4].try_into().unwrap_or([0; 4]),
                        );
                        offset += 4;
                        if size < 0 {
                            row.push(None);
                        } else {
                            let end = offset + size as usize;
                            row.push(Some(
                                String::from_utf8_lossy(&payload[offset..end]).to_string(),
                            ));
                            offset = end;
                        }
                    }
                    rows.push(row);
                }
                b'C' => {
                    tag = Some(
                        String::from_utf8_lossy(payload.strip_suffix(&[0]).unwrap_or(&payload))
                            .to_string(),
                    )
                }
                b'E' => {
                    let (code, message) = fields(&payload);
                    error = Some(PgError::Server { code, message });
                }
                b'Z' => {
                    return match error {
                        Some(error) => Err(error),
                        None => Ok((rows, tag)),
                    }
                }
                _ => {}
            }
        }
    }
}

impl Drop for Pg {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}
