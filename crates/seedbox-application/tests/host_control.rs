//! The CLI's host-control client against the engine's slice-boundary gate.
#![cfg(unix)]
use seedbox_engine_core::host_control as engine;

use std::{fs, os::unix::net::UnixDatagram, sync::mpsc, time::Duration};

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
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    // Queue before entering the boundary, then prove no simulated work follows it.
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
        assert!(observed.recv_timeout(Duration::from_millis(30)).is_err());
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
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
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
    let clock = seedbox_engine_protocol::host_clock::Reader::create(
        &endpoint.with_file_name(seedbox_engine_protocol::host_clock::FILE_NAME),
    )
    .unwrap();
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
        std::thread::sleep(Duration::from_millis(30));
        assert!(clock.waited().unwrap() >= Duration::from_millis(20));
        let step = client
            .execute(Command::Step, Duration::from_secs(2))
            .unwrap();
        assert_eq!(step.error, None);
        assert!(step.stepping);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = client
                .execute(Command::Status, Duration::from_secs(2))
                .unwrap();
            if status.position.completed_rounds == 1 {
                assert!(!status.stepping);
                assert_eq!(status.rate, PlaybackRate::Paused);
                break;
            }
            assert!(std::time::Instant::now() < deadline);
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
        let waited = clock.waited().unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(clock.waited().unwrap(), waited);
    });
}
