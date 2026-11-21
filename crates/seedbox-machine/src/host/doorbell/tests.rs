use super::*;
use std::time::Duration;

/// Run `body` on its own thread, so a lost wakeup fails the test instead of
/// hanging it.
fn bounded<T: Send + 'static>(reason: &str, body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        sender.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)))
    });
    seedbox_test_time::HangGuard::new(reason, Duration::from_secs(120))
        .recv(&receiver)
        .unwrap()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

#[test]
fn a_ring_after_the_mark_is_seen_on_the_word() {
    let bell = Doorbell::create().unwrap();
    let ringer = bell.ringer().unwrap();
    let mut coordinator = bell.claim().unwrap();
    coordinator.mark();
    assert!(!coordinator.rung());
    ringer.ring().unwrap();
    assert!(coordinator.rung());
    coordinator.mark();
    assert!(!coordinator.rung());
}

#[test]
fn one_coordinator_at_a_time() {
    let bell = Doorbell::create().unwrap();
    let first = bell.claim().unwrap();
    assert!(bell.claim().is_none());
    drop(first);
    assert!(bell.claim().is_some());
}

#[test]
fn every_park_ends_at_a_ring_from_another_thread() {
    let bell = Doorbell::create().unwrap();
    bounded("each ring wakes the parked coordinator", move || {
        let mut coordinator = bell.claim().unwrap();
        for _ in 0..2000 {
            coordinator.mark();
            let ringer = bell.ringer().unwrap();
            let ring = std::thread::spawn(move || ringer.ring().unwrap());
            // A park that returns without the ring is an early byte; the
            // ring itself always ends one.
            while !coordinator.rung() {
                assert_eq!(coordinator.park(&[]).unwrap(), None);
            }
            ring.join().unwrap();
        }
        let mut fd = libc::pollfd {
            fd: bell.read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, zero timeout checks the pipe state.
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 0) }, 0);
    });
}

#[test]
fn a_park_ends_when_a_watched_executor_exits() {
    let bell = Doorbell::create().unwrap();
    let (read, write) = std::io::pipe().unwrap();
    let (_live, _keep) = std::io::pipe().unwrap();
    bounded("a hang-up ends the park", move || {
        let mut coordinator = bell.claim().unwrap();
        coordinator.mark();
        drop(write);
        let fired = coordinator
            .park(&[_live.as_raw_fd(), read.as_raw_fd()])
            .unwrap();
        assert_eq!(fired, Some(1));
    });
}

#[test]
fn an_executor_inherits_the_doorbell_at_its_descriptors() {
    let bell = Doorbell::create().unwrap();
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "test -e /dev/fd/{REGION_DESCRIPTOR} && test -e /dev/fd/{BELL_DESCRIPTOR}"
    ));
    bell.inherit(&mut command).unwrap();
    assert!(command.status().unwrap().success());
}
