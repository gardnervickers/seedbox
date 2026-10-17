//! The CLI's host-control client against the engine's slice-boundary gate.
#![cfg(unix)]
use seedbox_engine_core::host_control as engine;

use seedbox_engine_protocol::host_clock;
use seedbox_test_time::HangGuard;
use std::{fs, os::unix::net::UnixDatagram, sync::mpsc, time::Duration};

/// Two wait-clock samples around an interval the test measures with the same
/// monotonic clock: (growth of the accounted wait, the measured interval).
/// The interval is strictly positive, and an open wait grows by at least it:
/// the first sample precedes the interval and the second follows it.
fn bracket(clock: &host_clock::Reader) -> (Duration, Duration) {
    let first = clock.waited().unwrap();
    // test-time: allow(host-time-subject) -- Sample the host-wait accounting clock itself to bracket its open/closed accounting transitions.
    let start = host_clock::Instant::now().unwrap();
    // test-time: allow(host-time-subject) -- Obtain a strictly positive interval of the host clock under test, not an ordering delay.
    while start.elapsed().unwrap().is_zero() {}
    // test-time: allow(host-time-subject) -- Compare accounted host wait with the same measured clock interval; no speed threshold is asserted.
    let interval = start.elapsed().unwrap();
    let second = clock.waited().unwrap();
    (second.checked_sub(first).unwrap(), interval)
}

#[test]
fn pause_acknowledges_a_boundary_and_repeated_requests_preserve_the_gate() {
    let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let run = directory.path().join("run");
    fs::create_dir_all(run.join("output")).unwrap();
    let socket = directory.path().join("engine");
    fs::write(
        run.join("output/host-control"),
        socket.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let mut control = engine::HostControl::bind(&socket).unwrap();
    let client = UnixDatagram::bind(directory.path().join("client")).unwrap();
    client.connect(&socket).unwrap();
    let guard = HangGuard::new(
        "host-control socket acknowledges actual commands",
        Duration::from_secs(2),
    );
    // test-time: allow(hang-guard) -- Bound positive pause/resume/invalid replies; payloads and gate ownership prove ordering.
    client
        .set_read_timeout(Some(guard.remaining().unwrap()))
        .unwrap();
    // Queue before entering the boundary, then prove no simulated work follows it.
    // Only the boundary loop answers, so each reply below proves the worker
    // was still inside the held boundary when it replied; nothing resumes it
    // before `set_paused(false)`, so it cannot have returned in between.
    client.send(b"pause").unwrap();
    let (progress, observed) = mpsc::channel();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            control.boundary().unwrap();
            progress.send(()).unwrap();
            control
        });
        let mut reply = [0; 32];
        let n = client.recv(&mut reply).unwrap();
        assert_eq!(&reply[..n], b"paused");
        assert!(observed.try_recv().is_err());
        seedbox_application::host_control::set_paused(&run, true).unwrap();
        assert!(observed.try_recv().is_err());
        client.send(b"invalid").unwrap();
        let n = client.recv(&mut reply).unwrap();
        assert_eq!(&reply[..n], b"invalid");
        assert!(observed.try_recv().is_err());
        // Lost reply clients must not turn a host observation into an engine error.
        let lost_path = directory.path().join("lost");
        let lost = UnixDatagram::bind(&lost_path).unwrap();
        lost.send_to(b"pause", &socket).unwrap();
        drop(lost);
        fs::remove_file(lost_path).unwrap();
        seedbox_application::host_control::set_paused(&run, false).unwrap();
        // Hang guard only: the resume releases the boundary.
        seedbox_test_time::HangGuard::new("host-control boundary completion after explicit resume on observed; retain the existing loaded-host deadlock budget, never timed absence", Duration::from_secs(2)).recv(&observed).unwrap();
        let mut control = worker.join().unwrap();
        // Resume is also idempotent when already running.
        client.send(b"resume").unwrap();
        control.boundary().unwrap();
        let n = client.recv(&mut reply).unwrap();
        assert_eq!(&reply[..n], b"running");
        drop(control);
    });
    assert!(!socket.exists());
    assert!(seedbox_application::host_control::set_paused(&run, true).is_err());
}

#[test]
fn typed_controls_resolve_a_timeout_before_admitting_a_step() {
    use engine::{Boundary, BoundaryPosition, Command, PlaybackRate};
    use seedbox_application::host_control::{Client, ClientError};
    let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let endpoint = directory.path().join("engine");
    let clock =
        host_clock::Reader::create(&endpoint.with_file_name(host_clock::FILE_NAME)).unwrap();
    let mut gate = engine::HostControl::bind(&endpoint).unwrap();
    let mut client = Client::connect(&endpoint).unwrap();
    let pause = Command::SetRate {
        rate: PlaybackRate::Paused,
    };
    assert!(matches!(
        client.execute(pause.clone(), Duration::from_millis(5)),
        Err(ClientError::Timeout { id: 1 })
    ));
    assert!(matches!(
        client.execute(Command::Step, Duration::from_millis(5)),
        Err(ClientError::Pending { id: 1 })
    ));
    assert_eq!(client.pending().unwrap().command, pause);

    // Unwind must release the boundary gate before scoped-thread joining.
    struct Resume<'a>(&'a UnixDatagram);
    impl Drop for Resume<'_> {
        fn drop(&mut self) {
            let _ = self.0.send(engine::RESUME);
        }
    }
    let cleanup = UnixDatagram::bind(directory.path().join("cleanup")).unwrap();
    cleanup.connect(&endpoint).unwrap();
    cleanup.set_nonblocking(true).unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            gate.boundary_at(Boundary::default()).unwrap();
            gate.boundary_at(Boundary {
                position: BoundaryPosition {
                    boundary_index: 1,
                    completed_rounds: 1,
                    simulated_time_ns: 10,
                },
                terminal: false,
                application_ready: false,
            })
            .unwrap();
        });
        let _resume = Resume(&cleanup);
        let paused = client.execute(pause, Duration::from_secs(2)).unwrap();
        assert_eq!(paused.id, 1);
        assert_eq!(paused.rate, PlaybackRate::Paused);
        assert!(client.pending().is_none());
        // The paused gate opens its accounted wait after replying, before it
        // first sleeps; it stays open until resumed. Wait for that transition
        // (the deadline is a hang guard), then prove the open wait accrues
        // host time across a further measured interval.
        HangGuard::new(
            "host-control opens its accounted wait",
            Duration::from_secs(2),
        )
        .wait_for(|| (!bracket(&clock).0.is_zero()).then_some(()))
        .expect("wait never opened");
        let (grown, interval) = bracket(&clock);
        assert!(grown >= interval, "{grown:?} < {interval:?}");
        let step = client
            .execute(Command::Step, Duration::from_secs(2))
            .unwrap();
        assert_eq!(step.error, None);
        assert!(step.stepping);
        // Hang guard only: the admitted step completes one round.
        let guard = HangGuard::new(
            "admitted host-control step completes one round",
            Duration::from_secs(2),
        );
        loop {
            let status = client
                .execute(Command::Status, Duration::from_secs(2))
                .unwrap();
            if status.position.completed_rounds == 1 {
                assert!(!status.stepping);
                assert_eq!(status.rate, PlaybackRate::Paused);
                break;
            }
            // Client execution is a blocking adapter, so keep it outside a wait_for observer.
            let _ = guard.remaining().expect("admitted step did not complete");
        }
        client
            .execute(
                Command::SetRate {
                    rate: PlaybackRate::Unlimited,
                },
                Duration::from_secs(2),
            )
            .unwrap();
        worker.join().unwrap();
        // Once released, the wait is closed: it no longer grows across a
        // strictly positive interval.
        assert!(clock.waited().unwrap() > Duration::ZERO);
        assert_eq!(bracket(&clock).0, Duration::ZERO);
    });
}
