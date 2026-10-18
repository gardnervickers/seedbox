//! Nonblocking transport only. Driver owns playback and simulation policy;
//! transport owns duplicate-request receipts and the shared host-wait clock.
use seedbox_engine_protocol::host_control::{self as wire, Reply, Request, MAX_DATAGRAM};
use std::{
    io,
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
};

pub(super) enum Incoming {
    Request {
        request: Box<Request>,
        peer: PathBuf,
    },
    Pause(PathBuf),
    Resume(PathBuf),
}
pub(super) struct Control {
    socket: UnixDatagram,
    path: PathBuf,
    last: Option<(Request, Reply)>,
    wait_clock: Option<seedbox_engine_protocol::host_clock::Writer>,
    waiting: bool,
}
impl Control {
    pub(super) fn bind(path: &Path) -> io::Result<Self> {
        let socket = UnixDatagram::bind(path)?;
        socket.set_nonblocking(true)?;
        let wait_clock = match seedbox_engine_protocol::host_clock::Writer::open(
            &path.with_file_name(seedbox_engine_protocol::host_clock::FILE_NAME),
        ) {
            Ok(clock) => Some(clock),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        Ok(Self {
            socket,
            path: path.into(),
            last: None,
            wait_clock,
            waiting: false,
        })
    }
    /// One datagram per call. The worker bounds each boundary drain to 64.
    pub(super) fn receive(&mut self) -> io::Result<Option<Incoming>> {
        let mut bytes = [0; MAX_DATAGRAM + 1];
        let (size, peer) = match self.socket.recv_from(&mut bytes) {
            Ok(message) => message,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(peer) = peer.as_pathname() else {
            return Ok(None);
        };
        let peer = peer.to_owned();
        let message = match &bytes[..size] {
            wire::PAUSE => Incoming::Pause(peer),
            wire::RESUME => Incoming::Resume(peer),
            bytes if size <= MAX_DATAGRAM => match serde_json::from_slice(bytes) {
                Ok(request) => Incoming::Request { request, peer },
                Err(_) => {
                    self.raw(&peer, wire::INVALID);
                    return Ok(None);
                }
            },
            _ => {
                self.raw(&peer, wire::INVALID);
                return Ok(None);
            }
        };
        Ok(Some(message))
    }
    /// Exact retries return their original reply; reusing an ID with another
    /// command or moving behind it never invokes the worker's mutation path.
    pub(super) fn prior(
        &self,
        request: &Request,
    ) -> Option<std::result::Result<Reply, wire::ControlError>> {
        let (previous, reply) = self.last.as_ref()?;
        if request == previous {
            return Some(Ok(reply.clone()));
        }
        if request.id == previous.id {
            return Some(Err(wire::ControlError::RequestConflict));
        }
        if request.id < previous.id {
            return Some(Err(wire::ControlError::StaleRequest));
        }
        None
    }
    pub(super) fn reply(&mut self, request: Request, reply: Reply, peer: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(&reply).map_err(io::Error::other)?;
        if bytes.len() > MAX_DATAGRAM {
            return Err(io::Error::other(
                "Driver control reply exceeds datagram limit",
            ));
        }
        self.last = Some((request, reply));
        self.raw(peer, &bytes);
        Ok(())
    }
    pub(super) fn cached(&self, reply: &Reply, peer: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(reply).map_err(io::Error::other)?;
        self.raw(peer, &bytes);
        Ok(())
    }
    pub(super) fn checkpoint_captured(&mut self, checkpoint: &wire::CheckpointStatus) {
        if let Some((_, reply)) = &mut self.last {
            reply.checkpoint = Some(checkpoint.clone());
        }
    }
    pub(super) fn checkpoint_refused(&mut self, detail: String) {
        if let Some((request, reply)) = &mut self.last {
            if matches!(request.command, wire::Command::Checkpoint { .. }) {
                reply.checkpoint = None;
                reply.error = Some(wire::ControlError::InvalidCheckpoint);
                reply.detail = Some(detail);
            }
        }
    }
    pub(super) fn raw(&self, peer: &Path, bytes: &[u8]) {
        let _ = self.socket.send_to(bytes, peer);
    }
    pub(super) fn waiting(&mut self, waiting: bool) -> io::Result<()> {
        if self.waiting != waiting {
            if let Some(clock) = &mut self.wait_clock {
                if waiting {
                    clock.begin_wait()?
                } else {
                    clock.end_wait()?
                }
            }
            self.waiting = waiting;
        }
        Ok(())
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        let _ = self.waiting(false);
        let _ = std::fs::remove_file(&self.path);
    }
}
