//! The CLI's host-control client against the engine's slice-boundary gate.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use seedbox_vmm::host_control as engine;

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
        seedbox::host_control::set_paused(&run, true).unwrap();
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
        seedbox::host_control::set_paused(&run, false).unwrap();
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
    assert!(seedbox::host_control::set_paused(&run, true).is_err());
}
