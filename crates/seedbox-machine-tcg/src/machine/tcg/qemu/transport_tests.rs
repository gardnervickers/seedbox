use super::*;

fn qmp() -> (Qmp, UnixStream) {
    let (ours, peer) = UnixStream::pair().unwrap();
    ours.set_read_timeout(Some(TIMEOUT)).unwrap();
    let qmp = Qmp {
        stream: BufReader::new(ours),
        stopped: false,
    };
    (qmp, peer)
}

#[test]
fn unbounded_wait_restores_the_read_timeout_after_success_error_and_panic() {
    let (mut qmp, _peer) = qmp();
    assert_eq!(
        qmp.without_read_timeout(|qmp| {
            assert_eq!(qmp.stream.get_ref().read_timeout().unwrap(), None);
            Ok(1)
        })
        .unwrap(),
        1
    );
    assert_eq!(qmp.stream.get_ref().read_timeout().unwrap(), Some(TIMEOUT));
    let error = qmp
        .without_read_timeout(|_| -> Result<(), Error> { Err(Error("failed".into())) })
        .unwrap_err();
    assert_eq!(error.to_string(), "failed");
    assert_eq!(qmp.stream.get_ref().read_timeout().unwrap(), Some(TIMEOUT));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = qmp.without_read_timeout(|_| -> Result<(), Error> { panic!("inside the wait") });
    }));
    assert!(panicked.is_err());
    assert_eq!(qmp.stream.get_ref().read_timeout().unwrap(), Some(TIMEOUT));
}

#[test]
fn bounded_run_wait_is_peer_gated_and_has_no_transport_deadline() {
    // No elapsed-time assertion: inspect the actual socket policy while the
    // controlled peer withholds every run response until explicitly released.
    for stop_before_reply in [false, true] {
        let (mut qmp, mut peer) = qmp();
        // Ordinary replies retain the standard hang guard. No deliberately
        // tiny timeout may make their success depend on peer scheduling.
        let budget = TIMEOUT;
        qmp.stream.get_ref().set_read_timeout(Some(budget)).unwrap();
        let inspect = qmp.stream.get_ref().try_clone().unwrap();
        // A fixture transport hang guard, not the duration of a successful run.
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        let (seen, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            // Keep the release sender inside the scope body: assertion unwind
            // drops it before scoped threads are joined, releasing the peer.
            let (release, released) = std::sync::mpsc::channel();
            let emulator = scope.spawn(move || {
                let mut input = BufReader::new(peer.try_clone().unwrap());
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                let command: Value = serde_json::from_str(&line).unwrap();
                seen.send(command).unwrap();
                // Dropping release on assertion failure also unblocks this peer.
                if released.recv().is_err() {
                    return;
                }
                if stop_before_reply {
                    writeln!(peer, "{}", json!({"event":"STOP"})).unwrap();
                }
                writeln!(peer, "{}", json!({"return":{}})).unwrap();
                if !stop_before_reply {
                    writeln!(peer, "{}", json!({"event":"STOP"})).unwrap();
                }
                line.clear();
                input.read_line(&mut line).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap(),
                    json!({"execute":"query-seedbox-run", "arguments":{}})
                );
                writeln!(
                    peer,
                    "{}",
                    json!({"return":{"time":22,"reason":"boundary"}})
                )
                .unwrap();
            });
            let run = scope.spawn(|| {
                qmp.execute_bounded(
                    Timestamp::from_nanos(2),
                    Timestamp::from_nanos(22),
                    crate::execution::ExecutionConfig::default(),
                )
            });
            // The guard only diagnoses a broken fixture; receiving the request
            // and inspecting SO_RCVTIMEO are the behavioural proof.
            let command = received.recv_timeout(TIMEOUT).unwrap();
            assert_eq!(
                command,
                json!({"execute":"seedbox-run", "arguments":{
                "time":2,"budget":20,"factor":1000}})
            );
            assert_eq!(inspect.read_timeout().unwrap(), None);
            release.send(()).unwrap();
            assert_eq!(
                run.join().unwrap().unwrap(),
                json!({"time":22,"reason":"boundary"})
            );
            emulator.join().unwrap();
        });
        assert_eq!(inspect.read_timeout().unwrap(), Some(budget));
    }
}
