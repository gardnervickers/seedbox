use super::*;

const DATAGRAM: usize = observation::MAX_DATAGRAM_BYTES;

/// Send filler datagrams on `socket` until a nonblocking send reports the
/// receiver full; returns how many are waiting. Darwin bounds the receiver's
/// buffer, Linux the undelivered bytes of each sender, so fill from the
/// socket under test to be full on both.
fn fill_from(socket: &UnixDatagram) -> usize {
    let mut held = 0;
    loop {
        match socket.send(&[0; DATAGRAM]) {
            Ok(_) => held += 1,
            Err(error) if receiver_full(&error) => return held,
            Err(error) => panic!("filler send: {error}"),
        }
    }
}

#[test]
fn mandatory_observations_wait_for_a_live_observer_to_drain_a_full_queue() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.sock");
    let receiver = UnixDatagram::bind(&path).unwrap();
    observation::configure_socket(&receiver).unwrap();
    // Containment only: a lost datagram would leave the reader waiting.
    receiver
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut sender = Sender::connect(&path, Duration::from_secs(60)).unwrap();
    let held = fill_from(&sender.socket);
    assert!(held > 0);
    // Far more than the receive buffer holds, the first sent while it is full.
    const COUNT: u32 = 1024;
    let (waiting, waits) = std::sync::mpsc::channel();
    sender.waiting = Some(waiting);
    let publisher = std::thread::spawn(move || {
        let sent: Vec<_> = (0..COUNT)
            .map(|n| {
                let mut bytes = vec![1; DATAGRAM];
                bytes[..4].copy_from_slice(&n.to_le_bytes());
                sender.send(&bytes, true)
            })
            .collect();
        (sent, sender.expired)
    });
    // Drain only once the first observation is waiting on the full queue,
    // rather than dropped.
    waits
        .recv_timeout(Duration::from_secs(60))
        .expect("a send into a full queue waits for the observer");
    let mut buffer = vec![0; DATAGRAM];
    for _ in 0..held {
        assert_eq!(receiver.recv(&mut buffer).unwrap(), DATAGRAM);
        assert_eq!(buffer[0], 0);
    }
    for n in 0..COUNT {
        assert_eq!(receiver.recv(&mut buffer).unwrap(), DATAGRAM);
        assert_eq!(buffer[..4], n.to_le_bytes());
    }
    let (sent, expired) = publisher.join().unwrap();
    assert!(sent.into_iter().all(|ok| ok));
    assert_eq!(expired, 0);
}

#[test]
fn a_stopped_observer_delays_once_and_frames_never_wait() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.sock");
    let receiver = UnixDatagram::bind(&path).unwrap();
    observation::configure_socket(&receiver).unwrap();
    receiver.set_nonblocking(true).unwrap();
    let mut sender = Sender::connect(&path, Duration::from_millis(50)).unwrap();
    let held = fill_from(&sender.socket);
    // Optional frames try once and never start a wait.
    assert!(!sender.send(&[2; DATAGRAM], false));
    assert_eq!((sender.expired, sender.stalled), (0, false));
    // The first mandatory send waits out the stall; later ones try once.
    for _ in 0..4096 {
        assert!(!sender.send(&[3; DATAGRAM], true));
    }
    assert_eq!((sender.expired, sender.stalled), (1, true));
    // Once the observer drains, the next datagram is accepted and ends the stall.
    let mut buffer = vec![0; DATAGRAM];
    for _ in 0..held {
        receiver.recv(&mut buffer).unwrap();
    }
    assert!(sender.send(&[4; DATAGRAM], true));
    assert!(!sender.stalled);
    assert_eq!(receiver.recv(&mut buffer).unwrap(), DATAGRAM);
    assert_eq!(buffer[0], 4);
}
