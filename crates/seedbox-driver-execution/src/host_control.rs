//! Host-control endpoint ownership and the server's worker control client.

use crate::Result;
use std::{fs, path::Path, time::Duration};

const REFERENCE: &str = "host-control";

mod client;
pub use client::{Client, ClientError};

/// Own the live endpoint locator for exactly one executor child lifetime.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct Endpoint {
    directory: tempfile::TempDir,
    reference: std::path::PathBuf,
    clock: seedbox_engine_protocol::host_clock::Reader,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Endpoint {
    pub(crate) fn new(output: &Path) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("seedbox-host-")
            .tempdir_in("/tmp")?;
        let clock = seedbox_engine_protocol::host_clock::Reader::create(
            &directory
                .path()
                .join(seedbox_engine_protocol::host_clock::FILE_NAME),
        )?;
        let endpoint = Self {
            directory,
            clock,
            reference: output.join(REFERENCE),
        };
        fs::write(
            &endpoint.reference,
            endpoint.socket().as_os_str().as_encoded_bytes(),
        )?;
        Ok(endpoint)
    }

    pub(crate) fn waited(&self) -> Result<Duration> {
        Ok(self.clock.waited()?)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

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
