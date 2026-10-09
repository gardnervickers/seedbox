//! Pause the host's whole world without sending an in-guest control request.

use crate::{Error, Result};
use seedbox_engine_core::host_control::{MAX_DATAGRAM, PAUSE, PAUSED, RESUME, RUNNING};
use std::{fs, os::unix::net::UnixDatagram, path::Path, time::Duration};

const REFERENCE: &str = "host-control";

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Paused,
    Resumed,
}

/// Request a slice-boundary pause or resume and wait for the engine's acknowledgement.
/// Repeated requests are idempotent. Concurrent requests apply in receive order.
pub fn set_paused(run: &Path, paused: bool) -> Result<Status> {
    let failure = |detail: &str| {
        Error::Setup(format!(
            "host control {}: {detail}; use an executing run/replay with a host-control engine and the same user (or root)",
            run.display()
        ))
    };
    let reference = fs::read_to_string(run.join("output").join(REFERENCE))
        .map_err(|_| failure("not available"))?;
    // Short private paths also work when the run directory exceeds sun_path's limit.
    let directory = tempfile::Builder::new()
        .prefix("seedbox-control-")
        .tempdir_in("/tmp")
        .map_err(|e| failure(&format!("cannot create reply directory: {e}")))?;
    let socket = UnixDatagram::bind(directory.path().join("reply"))
        .map_err(|e| failure(&format!("cannot bind reply socket: {e}")))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| failure(&e.to_string()))?;
    socket
        .connect(reference.trim())
        .map_err(|_| failure("not available"))?;
    // A full datagram queue during boot must not block the controlling CLI.
    socket
        .set_nonblocking(true)
        .map_err(|e| failure(&e.to_string()))?;
    socket
        .send(if paused { PAUSE } else { RESUME })
        .map_err(|_| failure("not available or request queue full"))?;
    socket
        .set_nonblocking(false)
        .map_err(|e| failure(&e.to_string()))?;
    let mut reply = [0; MAX_DATAGRAM];
    let size = socket.recv(&mut reply).map_err(|_| {
        failure("no boundary acknowledgement within 5s (request may still take effect)")
    })?;
    let expected = if paused { PAUSED } else { RUNNING };
    if &reply[..size] != expected {
        return Err(failure("invalid boundary acknowledgement"));
    }
    Ok(if paused {
        Status::Paused
    } else {
        Status::Resumed
    })
}

/// Own the live endpoint locator for exactly one executor child lifetime.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct Endpoint {
    directory: tempfile::TempDir,
    reference: std::path::PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Endpoint {
    pub(crate) fn new(output: &Path) -> Result<Self> {
        let endpoint = Self {
            directory: tempfile::Builder::new()
                .prefix("seedbox-host-")
                .tempdir_in("/tmp")?,
            reference: output.join(REFERENCE),
        };
        fs::write(
            &endpoint.reference,
            endpoint.socket().as_os_str().as_encoded_bytes(),
        )?;
        Ok(endpoint)
    }

    pub(crate) fn socket(&self) -> std::path::PathBuf {
        self.directory.path().join("socket")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.reference) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("host control cleanup {}: {error}", self.reference.display());
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_cleanup_removes_its_locator_and_private_socket_directory() {
        let output = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(output.path()).unwrap();
        let socket = endpoint.socket();
        assert_eq!(
            fs::read_to_string(output.path().join(REFERENCE)).unwrap(),
            socket.to_str().unwrap()
        );
        let server = UnixDatagram::bind(&socket).unwrap();
        drop(server); // A killed child can leave its socket pathname behind.
        drop(endpoint);
        assert!(!output.path().join(REFERENCE).exists());
        assert!(!socket.parent().unwrap().exists());
    }
}
