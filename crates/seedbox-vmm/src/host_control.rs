//! Host execution control, checked only between world slices. No guest state.

use std::{
    io,
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct HostControl {
    socket: UnixDatagram,
    path: PathBuf,
    paused: bool,
}

impl HostControl {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let socket = UnixDatagram::bind(path)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            path: path.to_owned(),
            paused: false,
        })
    }

    /// Acknowledge after reaching a boundary; stay here until a resume request.
    /// The executor continues enforcing cancellation and its wall deadline.
    pub fn boundary(&mut self) -> io::Result<()> {
        loop {
            // Bound each drain so senders cannot indefinitely hold a running world.
            for _ in 0..64 {
                let mut bytes = [0u8; 32];
                let (size, peer) = match self.socket.recv_from(&mut bytes) {
                    Ok(message) => message,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                let Some(peer) = peer.as_pathname() else {
                    continue;
                };
                let reply: &[u8] = match &bytes[..size] {
                    b"pause" => {
                        self.paused = true;
                        b"paused"
                    }
                    b"resume" => {
                        self.paused = false;
                        b"running"
                    }
                    _ => b"invalid",
                };
                // A disappearing requester must never fail the guest execution.
                let _ = self.socket.send_to(reply, peer);
            }
            if !self.paused {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for HostControl {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
