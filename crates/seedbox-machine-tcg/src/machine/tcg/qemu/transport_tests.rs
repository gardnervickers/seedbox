use super::*;

fn qmp() -> (Qmp, UnixStream) {
    let (ours, peer) = UnixStream::pair().unwrap();
    ours.set_read_timeout(Some(TIMEOUT)).unwrap();
    let qmp = Qmp {
        stream: BufReader::new(ours),
        stopped: false,
        request: Vec::new(),
    };
    (qmp, peer)
}

#[test]
fn commands_preserve_framing_values_and_errors_across_reused_requests() {
    let (mut qmp, peer) = qmp();
    peer.set_read_timeout(Some(TIMEOUT)).unwrap();
    std::thread::scope(|scope| {
        let emulator = scope.spawn(move || {
            let mut peer = BufReader::new(peer);
            for (name, arguments, response) in [
                (
                    "first",
                    json!({"text": "line\n\"quoted\" \\ λ", "bytes": [0, 255], "large": u64::MAX}),
                    json!({"return": {"accepted": true}}),
                ),
                ("bad", json!({}), json!({"error": {"desc": "rejected"}})),
                ("last", json!({}), json!({"return": []})),
            ] {
                let mut line = String::new();
                peer.read_line(&mut line).unwrap();
                assert!(line.ends_with('\n'));
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap(),
                    json!({"execute": name, "arguments": arguments})
                );
                writeln!(peer.get_mut(), "{}", json!({"event": "STOP"})).unwrap();
                writeln!(peer.get_mut(), "{response}").unwrap();
            }
        });
        assert_eq!(
            qmp.command(
                "first",
                json!({"text": "line\n\"quoted\" \\ λ", "bytes": [0, 255], "large": u64::MAX})
            )
            .unwrap(),
            json!({"accepted": true})
        );
        assert!(qmp.stopped);
        assert!(qmp
            .command("bad", json!({}))
            .unwrap_err()
            .to_string()
            .contains("rejected"));
        assert_eq!(qmp.command("last", json!({})).unwrap(), json!([]));
        emulator.join().unwrap();
    });
}

#[test]
#[ignore = "requires native QEMU"]
fn native_qmp_accepts_fragmented_large_requests_and_recovers_after_command_errors() {
    let manifest = Manifest {
        boot: Boot::Program(super::super::raw_program_data::counting_loop().into()),
        memory_mib: 16,
        disk: None,
        network: None,
    };
    let mut process =
        Process::start(Path::new("qemu-system-x86_64"), &manifest, None, 7, None).unwrap();
    // Each request is single-flight, as in production. The escaped, UTF-8 ID
    // spans several bounded QEMU reads and must return byte-for-byte intact.
    for name in ["query-status", "no-such-seedbox-command", "query-status"] {
        let id = "line\n\"quoted\" \\ λ".repeat(1024);
        let mut request = serde_json::to_vec(&json!({
            "execute": name, "arguments": {}, "id": id,
        }))
        .unwrap();
        request.push(b'\n');
        let stream = process.qmp.stream.get_mut();
        stream.write_all(&request[..17]).unwrap();
        stream.write_all(&request[17..]).unwrap();
        let response = loop {
            let value = process.qmp.read().unwrap();
            if value.get("event").is_none() {
                break value;
            }
        };
        assert_eq!(response["id"], id);
        if name == "query-status" {
            assert_eq!(response["return"]["running"], false);
        } else {
            assert_eq!(response["error"]["class"], "CommandNotFound");
        }
    }
    assert_eq!(
        process
            .execute(
                Timestamp::from_nanos(0),
                Timestamp::from_nanos(10_000),
                seedbox_machine::execution::ExecutionConfig::default(),
            )
            .unwrap(),
        json!({"time": 10_000, "reason": "boundary"})
    );
}

#[test]
#[ignore = "requires native QEMU"]
fn native_fused_drain_preserves_empty_inline_and_deferred_console_journals() {
    use super::super::raw_program_data::{Asm, Cond, Reg, STUB};
    use seedbox_machine::pv::transport::{GuestWrite, LifecycleWrite};

    // Console records occupy 13 framing bytes plus one payload byte: these
    // two populations bracket 64 KiB by one whole record. Payload alone is
    // much smaller in both cases, so a payload-only limit fails this oracle.
    for count in [0, 1, 4681, 4682] {
        let mut program = Asm::default();
        if count != 0 {
            program
                .mov_imm(Reg::Edx, 0x3f8)
                .mov_al(0xff)
                .mov_imm(Reg::Ecx, count)
                .label("console")
                .out_dx()
                .dec(Reg::Ecx)
                .jcc(Cond::NotEqual, "console");
        }
        let manifest = Manifest {
            boot: Boot::Program(program.spin().image().into()),
            memory_mib: 16,
            disk: None,
            network: None,
        };
        let qemu = Path::new("qemu-system-x86_64");
        let mut process = Process::start(qemu, &manifest, None, 7, None).unwrap();
        let end = STUB + 4 + u64::from(count) * 3;
        let stopped = process
            .execute(
                Timestamp::from_nanos(0),
                Timestamp::from_nanos(end),
                Default::default(),
            )
            .unwrap();
        let expected: Vec<_> = (0..count)
            .map(|i| {
                GuestWrite::Console(LifecycleWrite {
                    time: STUB + 4 + u64::from(i) * 3,
                    bytes: vec![0xff],
                })
            })
            .collect();
        let reply = process
            .qmp
            .command("seedbox-drain-run", json!({"network":false}))
            .unwrap();
        assert_eq!(reply["run"], stopped);
        assert!(reply.get("network").is_none());
        if count <= 4681 {
            assert_eq!(guest_writes(reply["writes"].clone()).unwrap(), expected);
        } else {
            assert!(reply.get("writes").is_none());
            // A second query cannot silently consume a deferred journal.
            let again = process
                .qmp
                .command("seedbox-drain-run", json!({"network":false}))
                .unwrap();
            assert!(again.get("writes").is_none());
            let snapshot = process.snapshot().unwrap();
            assert_eq!(process.drain_guest().unwrap(), expected);
            let mut restored = Process::start(qemu, &manifest, Some(&snapshot), 7, None).unwrap();
            assert_eq!(restored.drain_guest().unwrap(), expected);
            assert!(restored.drain_guest().unwrap().is_empty());
        }
        assert!(process.drain_guest().unwrap().is_empty());
        let empty = process
            .qmp
            .command("seedbox-drain-run", json!({"network":false}))
            .unwrap();
        assert_eq!(empty["writes"], json!([]));
        assert_eq!(empty["run"], stopped);
        assert_eq!(process.console().unwrap(), vec![0xff; count as usize]);
        assert!(process
            .qmp
            .command("seedbox-drain-run", json!({"network":true}))
            .unwrap_err()
            .to_string()
            .contains("seedbox-net-drain"));
    }
}

#[test]
fn fused_nic_first_chunk_keeps_continuation_order_and_command_errors() {
    let first = json!({"frames": (0..256).map(|time| json!({
        "time":time, "bytes":"00".repeat(60)
    })).collect::<Vec<_>>(), "more":true});
    for (response, expected_error) in [
        (
            json!({"return":{"frames":[{"time":256,"bytes":"ff".repeat(1514)}],"more":false}}),
            None,
        ),
        (
            json!({"return":{"frames":[{"time":254,"bytes":"00".repeat(60)}],"more":false}}),
            Some("invalid seedbox-net-drain"),
        ),
        (
            json!({"error":{"desc":"device failed"}}),
            Some("device failed"),
        ),
    ] {
        let (mut qmp, peer) = qmp();
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        std::thread::scope(|scope| {
            let emulator = scope.spawn(move || {
                let mut peer = BufReader::new(peer);
                let mut line = String::new();
                peer.read_line(&mut line).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap(),
                    json!({"execute":"seedbox-net-drain","arguments":{}})
                );
                writeln!(peer.get_mut(), "{response}").unwrap();
            });
            let result = qmp.net_drain(first.clone());
            match expected_error {
                None => {
                    let frames = result.unwrap();
                    assert_eq!(frames.len(), 257);
                    assert_eq!(frames[255].time, 255);
                    assert_eq!(frames[256].time, 256);
                    assert_eq!(frames[256].bytes, vec![0xff; 1514]);
                }
                Some(expected) => assert!(result.unwrap_err().to_string().contains(expected)),
            }
            emulator.join().unwrap();
        });
    }
    let (mut qmp, peer) = qmp();
    drop(peer);
    assert!(qmp
        .net_drain(json!({"frames":[],"more":false}))
        .unwrap()
        .is_empty());
    for invalid in [
        json!({"frames":[],"more":true}),
        json!({"frames":[{"time":0,"bytes":"00".repeat(13)}],"more":false}),
    ] {
        assert!(qmp.net_drain(invalid).is_err());
    }
}

#[test]
fn fused_collection_distinguishes_empty_and_deferred_and_rejects_malformed_output() {
    use seedbox_machine::pv::transport::{GuestWrite, LifecycleWrite};
    let stopped = json!({"time":5,"reason":"boundary"});
    let (mut empty_qmp, peer) = qmp();
    drop(peer);
    let empty = empty_qmp
        .collect_run(json!({"run":stopped,"writes":[]}), false)
        .unwrap();
    assert_eq!(empty.result, stopped);
    assert!(empty.writes.is_empty() && empty.frames.is_empty());
    for (reply, network) in [
        (json!({"writes":[]}), false),
        (json!({"run":stopped,"writes":null}), false),
        (
            json!({"run":stopped,"writes":[{"kind":"console","time":1,"bytes":[256]}]}),
            false,
        ),
        (
            json!({"run":stopped,"writes":[],"network":{"frames":[],"more":false}}),
            false,
        ),
        (json!({"run":stopped,"writes":[]}), true),
        (json!({"run":stopped,"writes":[],"network":null}), true),
    ] {
        assert!(empty_qmp.collect_run(reply, network).is_err());
    }

    let (mut qmp, peer) = qmp();
    peer.set_read_timeout(Some(TIMEOUT)).unwrap();
    std::thread::scope(|scope| {
        let emulator = scope.spawn(move || {
            let mut peer = BufReader::new(peer);
            for (name, response) in [
                (
                    "seedbox-drain-lifecycle",
                    json!([
                        {"kind":"console","time":2,"bytes":[0,255]},
                        {"kind":"lifecycle","time":2,"bytes":[1,2,3]},
                    ]),
                ),
                (
                    "seedbox-net-drain",
                    json!({"frames":[{"time":4,"bytes":"ff".repeat(60)}],"more":false}),
                ),
            ] {
                let mut line = String::new();
                peer.read_line(&mut line).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap(),
                    json!({"execute":name,"arguments":{}})
                );
                writeln!(peer.get_mut(), "{}", json!({"return":response})).unwrap();
            }
        });
        let output = qmp
            .collect_run(
                json!({"run":stopped,"network":{
                    "frames":[{"time":3,"bytes":"00".repeat(60)}],"more":true
                }}),
                true,
            )
            .unwrap();
        assert_eq!(output.result, stopped);
        assert_eq!(
            output.writes,
            [
                GuestWrite::Console(LifecycleWrite {
                    time: 2,
                    bytes: vec![0, 255]
                }),
                GuestWrite::Lifecycle(LifecycleWrite {
                    time: 2,
                    bytes: vec![1, 2, 3]
                }),
            ]
        );
        assert_eq!(
            output.frames,
            [
                LifecycleWrite {
                    time: 3,
                    bytes: vec![0; 60]
                },
                LifecycleWrite {
                    time: 4,
                    bytes: vec![255; 60]
                },
            ]
        );
        emulator.join().unwrap();
    });
}

#[test]
fn fused_run_errors_keep_single_flight_and_restore_the_query_timeout() {
    for fail_run in [false, true] {
        let (mut qmp, peer) = qmp();
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        assert_eq!(qmp.stream.get_ref().read_timeout().unwrap(), Some(TIMEOUT));
        std::thread::scope(|scope| {
            let (release, released) = std::sync::mpsc::channel();
            let emulator = scope.spawn(move || {
                let mut peer = BufReader::new(peer);
                let mut line = String::new();
                peer.read_line(&mut line).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["execute"],
                    "seedbox-run"
                );
                if !fail_run {
                    writeln!(peer.get_mut(), "{}", json!({"event":"STOP"})).unwrap();
                    writeln!(peer.get_mut(), "{}", json!({"return":{}})).unwrap();
                    line.clear();
                    peer.read_line(&mut line).unwrap();
                    assert_eq!(
                        serde_json::from_str::<Value>(&line).unwrap(),
                        json!({"execute":"seedbox-drain-run","arguments":{"network":true}})
                    );
                }
                writeln!(peer.get_mut(), "{}", json!({"error":{"desc":"failed"}})).unwrap();
                // Inspect restoration on the still-connected transport. On
                // assertion unwind, dropping release also unblocks this peer.
                let _ = released.recv();
            });
            let error = qmp
                .execute_bounded(
                    Timestamp::from_nanos(0),
                    Timestamp::from_nanos(1),
                    Default::default(),
                    Completion::Drain { network: true },
                )
                .unwrap_err();
            assert!(error.to_string().contains(if fail_run {
                "QMP seedbox-run:"
            } else {
                "QMP seedbox-drain-run:"
            }));
            assert_eq!(qmp.stream.get_ref().read_timeout().unwrap(), Some(TIMEOUT));
            release.send(()).unwrap();
            emulator.join().unwrap();
        });
    }
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
    for (stop_before_reply, network) in [false, true]
        .into_iter()
        .flat_map(|stop| [None, Some(false), Some(true)].map(|network| (stop, network)))
    {
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
                    match network {
                        None => json!({"execute":"query-seedbox-run", "arguments":{}}),
                        Some(network) => json!({"execute":"seedbox-drain-run",
                            "arguments":{"network":network}}),
                    }
                );
                writeln!(
                    peer,
                    "{}",
                    json!({"return": match network {
                        None => json!({"time":22,"reason":"boundary"}),
                        Some(false) => json!({"run":{"time":22,"reason":"boundary"},"writes":[]}),
                        Some(true) => json!({"run":{"time":22,"reason":"boundary"},
                            "writes":[],"network":{"frames":[],"more":false}}),
                    }})
                )
                .unwrap();
            });
            let run = scope.spawn(|| {
                qmp.execute_bounded(
                    Timestamp::from_nanos(2),
                    Timestamp::from_nanos(22),
                    seedbox_machine::execution::ExecutionConfig::default(),
                    match network {
                        None => Completion::Query,
                        Some(network) => Completion::Drain { network },
                    },
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
                match network {
                    None => json!({"time":22,"reason":"boundary"}),
                    Some(false) => json!({"run":{"time":22,"reason":"boundary"},"writes":[]}),
                    Some(true) => json!({"run":{"time":22,"reason":"boundary"},
                        "writes":[],"network":{"frames":[],"more":false}}),
                }
            );
            emulator.join().unwrap();
        });
        assert_eq!(inspect.read_timeout().unwrap(), Some(budget));
    }
}
