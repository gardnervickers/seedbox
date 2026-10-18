//! Feature-only injection through the original execution owner's channel.
//! No process discovery or stored PID can grant this authority.
use std::{
    io::{self, Write},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, Weak},
};

pub(super) type OwnerEndpoint = Arc<Mutex<UnixStream>>;
pub(super) const FAIL_WORKER: u8 = 0xf1;

#[derive(Default)]
struct Binding {
    bound: bool,
    endpoint: Option<Weak<Mutex<UnixStream>>>,
}

/// One-shot failure request for one original spawn-owned worker, available only
/// with `test-support`. The real monitor retains sole process/cleanup authority.
/// Clones are weak with respect to execution ownership: keeping this handle
/// alive cannot prevent owner EOF or cancellation. A handle cannot be rebound.
#[derive(Clone, Default)]
pub struct WorkerFailureInjector(Arc<Mutex<Binding>>);

impl WorkerFailureInjector {
    pub(super) fn bind(&self, owner: &OwnerEndpoint) -> io::Result<()> {
        let mut binding = self
            .0
            .lock()
            .map_err(|_| io::Error::other("failure injector poisoned"))?;
        if binding.bound {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "failure injector already bound to an execution",
            ));
        }
        binding.bound = true;
        binding.endpoint = Some(Arc::downgrade(owner));
        Ok(())
    }

    /// Request termination of the original worker only, not its monitor/anchor.
    /// Successful delivery is NOT cleanup proof: await ordinary execution failure
    /// and the monitor's normal receipt/barriers. The request is consumed once.
    pub fn fail_worker(&self) -> io::Result<()> {
        let endpoint = self
            .0
            .lock()
            .map_err(|_| io::Error::other("failure injector poisoned"))?
            .endpoint
            .take()
            .and_then(|endpoint| endpoint.upgrade())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "failure injector has no live original execution",
                )
            })?;
        let mut endpoint = endpoint
            .lock()
            .map_err(|_| io::Error::other("execution owner poisoned"))?;
        endpoint.write_all(&[FAIL_WORKER])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_is_one_shot_and_never_retargets_another_owner() {
        let injector = WorkerFailureInjector::default();
        assert_eq!(
            injector.fail_worker().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        let (owner, mut peer) = UnixStream::pair().unwrap();
        let owner = Arc::new(Mutex::new(owner));
        injector.bind(&owner).unwrap();
        injector.fail_worker().unwrap();
        let guard = seedbox_test_time::HangGuard::new(
            "positive delivery of the one-byte owned injection request",
            std::time::Duration::from_secs(30),
        );
        peer.set_read_timeout(Some(guard.remaining().unwrap()))
            .unwrap();
        let mut byte = [0];
        use std::io::Read;
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [FAIL_WORKER]);
        assert_eq!(
            injector.fail_worker().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(
            injector.bind(&owner).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn stale_handle_cannot_keep_original_owner_alive_or_bind_replacement() {
        let injector = WorkerFailureInjector::default();
        let (owner, _) = UnixStream::pair().unwrap();
        let owner = Arc::new(Mutex::new(owner));
        injector.bind(&owner).unwrap();
        drop(owner);
        assert_eq!(
            injector.fail_worker().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        let (replacement, _) = UnixStream::pair().unwrap();
        let replacement = Arc::new(Mutex::new(replacement));
        assert_eq!(
            injector.bind(&replacement).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }
}
