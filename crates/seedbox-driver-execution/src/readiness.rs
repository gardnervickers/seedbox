//! Waiting on descriptors instead of sleeping between polls.
use std::{io, os::fd::RawFd, time::Duration};

/// Block until one of `descriptors` is readable or closed, or until `timeout`
/// passes; `None` waits without a deadline. Returns whether a descriptor is
/// ready. A signal also ends the wait, so callers re-check their condition
/// after every return.
pub(crate) fn wait(descriptors: &[RawFd], timeout: Option<Duration>) -> io::Result<bool> {
    let mut polled: Vec<_> = descriptors
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // poll(2) counts whole milliseconds. Round up, so a remainder shorter
    // than one millisecond still blocks instead of spinning.
    let timeout = timeout.map_or(-1, |timeout| {
        timeout
            .as_nanos()
            .div_ceil(1_000_000)
            .min(libc::c_int::MAX as u128) as libc::c_int
    });
    match unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, timeout) } {
        -1 => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
        ready => Ok(ready > 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::fd::AsRawFd, os::unix::net::UnixStream};

    #[test]
    fn data_or_a_closed_peer_ends_the_wait() {
        let guard = seedbox_test_time::HangGuard::new(
            "a readable or closed descriptor ends the wait",
            Duration::from_secs(30),
        );
        let (idle, _idle_peer) = UnixStream::pair().unwrap();
        assert!(!wait(&[idle.as_raw_fd()], Some(Duration::ZERO)).unwrap());
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(b"x").unwrap();
        let both = [idle.as_raw_fd(), reader.as_raw_fd()];
        assert!(wait(&both, Some(guard.remaining().unwrap())).unwrap());
        let (closed, peer) = UnixStream::pair().unwrap();
        drop(peer);
        assert!(wait(&[closed.as_raw_fd()], Some(guard.remaining().unwrap())).unwrap());
    }
}
