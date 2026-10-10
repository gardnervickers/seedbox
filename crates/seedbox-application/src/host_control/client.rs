//! One serialized control connection per worker. A timeout never permits a new
//! command to overtake a request whose effect is still unknown.
use seedbox_engine_protocol::host_control::{Command, Reply, Request, INVALID, MAX_DATAGRAM};
use std::{
    fmt, io,
    os::unix::net::UnixDatagram,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum ClientError {
    Transport(io::Error),
    Protocol(String),
    Unsupported,
    Pending { id: u64 },
    Timeout { id: u64 },
    Exhausted,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "worker control transport: {error}"),
            Self::Protocol(message) => write!(f, "invalid worker control reply: {message}"),
            Self::Unsupported => f.write_str("worker does not support this control command"),
            Self::Pending { id } => write!(f, "worker control request {id} is unacknowledged; retry it before issuing another command"),
            Self::Timeout { id } => write!(f, "worker control request {id} was not acknowledged before the deadline and may still take effect"),
            Self::Exhausted => f.write_str("worker control request sequence exhausted"),
        }
    }
}
impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

pub struct Client {
    // Close the socket before removing its private reply directory.
    socket: UnixDatagram,
    _directory: tempfile::TempDir,
    last_id: u64,
    pending: Option<(Request, Vec<u8>)>,
}

impl Client {
    pub fn for_run(run: &Path) -> Result<Self, ClientError> {
        use io::Read;
        let file = std::fs::File::open(run.join("output").join(super::REFERENCE))?;
        let mut bytes = Vec::new();
        file.take(MAX_DATAGRAM as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_DATAGRAM {
            return Err(ClientError::Protocol("oversized endpoint reference".into()));
        }
        let endpoint =
            std::str::from_utf8(&bytes).map_err(|e| ClientError::Protocol(e.to_string()))?;
        Self::connect(Path::new(endpoint.trim()))
    }

    /// The caller owns worker identity/lifetime and supplies its exact endpoint.
    /// Keep this client for the entire worker lifetime, including failed requests.
    pub fn connect(endpoint: &Path) -> Result<Self, ClientError> {
        let directory = tempfile::Builder::new()
            .prefix("seedbox-control-")
            .tempdir_in("/tmp")?;
        let socket = UnixDatagram::bind(directory.path().join("reply"))?;
        socket.connect(endpoint)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            _directory: directory,
            last_id: 0,
            pending: None,
        })
    }

    pub fn pending(&self) -> Option<&Request> {
        self.pending.as_ref().map(|(request, _)| request)
    }

    /// Retry exactly the same wire request until acknowledged or the host
    /// deadline expires. Protocol rejections are successful deliveries and are
    /// returned in Reply.error. Transport errors retain the pending request.
    pub fn execute(&mut self, command: Command, timeout: Duration) -> Result<Reply, ClientError> {
        if let Some((request, _)) = &self.pending {
            if request.command != command {
                return Err(ClientError::Pending { id: request.id });
            }
        } else {
            let id = self.last_id.checked_add(1).ok_or(ClientError::Exhausted)?;
            let request = Request { id, command };
            let bytes =
                serde_json::to_vec(&request).map_err(|e| ClientError::Protocol(e.to_string()))?;
            if bytes.len() > MAX_DATAGRAM {
                return Err(ClientError::Protocol(
                    "control request exceeds datagram limit".into(),
                ));
            }
            self.last_id = id;
            self.pending = Some((request, bytes));
        }
        let started = Instant::now();
        let mut next_send = Duration::ZERO;
        let mut buffer = [0; MAX_DATAGRAM + 1];
        loop {
            let (request, bytes) = self
                .pending
                .as_ref()
                .ok_or_else(|| ClientError::Protocol("missing pending request".into()))?;
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(ClientError::Timeout { id: request.id });
            }
            if elapsed >= next_send {
                match self.socket.send(bytes) {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
                next_send = elapsed.saturating_add(Duration::from_millis(20));
            }
            // Bound stale-reply draining so a peer cannot starve the deadline.
            for _ in 0..64 {
                let size = match self.socket.recv(&mut buffer) {
                    Ok(size) => size,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                };
                if &buffer[..size] == INVALID {
                    // An explicit protocol refusal is known not to have applied.
                    self.pending = None;
                    return Err(ClientError::Unsupported);
                }
                if size > MAX_DATAGRAM {
                    return Err(ClientError::Protocol("oversized datagram".into()));
                }
                let reply: Reply = serde_json::from_slice(&buffer[..size])
                    .map_err(|e| ClientError::Protocol(e.to_string()))?;
                if reply.id < request.id {
                    continue;
                }
                if reply.id > request.id {
                    return Err(ClientError::Protocol("unexpected request ID".into()));
                }
                self.pending = None;
                return Ok(reply);
            }
            std::thread::sleep(
                timeout
                    .saturating_sub(started.elapsed())
                    .min(Duration::from_millis(2)),
            );
        }
    }
}
