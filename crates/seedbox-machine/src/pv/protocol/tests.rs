use super::*;
fn id(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}
fn write(time: u64, bytes: &[u8]) -> LifecycleWrite {
    LifecycleWrite {
        time,
        bytes: bytes.to_vec(),
    }
}
fn command(c: &mut Codec, n: u32, a: ApplicationCommand) -> Vec<u8> {
    c.request(&Request::Application {
        id: id(n),
        command: a,
    })
    .unwrap()
}
fn response(time: u64, response: Response) -> Event {
    Event::Response { time, response }
}
#[test]
fn commands_acknowledgments_and_reserved_ids() {
    let mut c = Codec::default();
    for (n, a, wire) in [
        (1, ApplicationCommand::Start, "start"),
        (2, ApplicationCommand::Stop, "stop"),
        (3, ApplicationCommand::Pause, "freeze"),
        (4, ApplicationCommand::Resume, "thaw"),
    ] {
        assert_eq!(command(&mut c, n, a), format!("{wire} {n}\n").into_bytes());
        assert_eq!(
            c.receive(&write(
                7,
                format!("SEEDBOX_EFFECT id={n} action={wire} guest_ns=999\n").as_bytes()
            ))
            .unwrap(),
            [response(
                7,
                Response::CommandAcknowledged {
                    id: id(n),
                    result: CommandResult::Completed
                }
            )]
        );
    }
    assert_eq!(
        c.request(&Request::Application {
            id: id(1),
            command: ApplicationCommand::Start
        }),
        Err(Error::ReusedId)
    );
}
#[test]
fn combined_fragmented_frames_keep_time_order_and_binary_bytes() {
    let mut c = Codec::default();
    command(&mut c, 1, ApplicationCommand::Pause);
    let first = c
        .receive(&write(5, b"SBX_READY1\nSBX_LOG1 1 00ff"))
        .unwrap();
    assert_eq!(first, [response(5, Response::Ready)]);
    let events=c.receive(&write(8,b"0a\nSBX_LOG1 2 7a00\nSEEDBOX_FAILURE id=1 action=freeze guest_ns=42\nSBX_EXIT1 status=23\n")).unwrap();
    assert_eq!(
        events,
        [
            response(
                8,
                Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes: vec![0, 255, 10]
                }
            ),
            response(
                8,
                Response::ApplicationOutput {
                    stream: Stream::Stderr,
                    bytes: vec![b'z', 0]
                }
            ),
            response(
                8,
                Response::CommandAcknowledged {
                    id: id(1),
                    result: CommandResult::Rejected
                }
            ),
            response(8, Response::ApplicationOutcome { code: 23 })
        ]
    );
}
#[test]
fn snapshot_retains_partial_and_command_until_matching_ack() {
    let mut c = Codec::default();
    command(&mut c, 3, ApplicationCommand::Stop);
    assert!(c
        .receive(&write(10, b"SEEDBOX_EFFECT id=3 action=st"))
        .unwrap()
        .is_empty());
    let mut c: Codec = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
    assert_eq!(c.pending.len(), 1);
    assert_eq!(
        c.receive(&write(11, b"op guest_ns=4\n")).unwrap(),
        [response(
            11,
            Response::CommandAcknowledged {
                id: id(3),
                result: CommandResult::Completed
            }
        )]
    );
    assert!(c.pending.is_empty());
    assert!(c.used_ids.contains(&id(3)));
}
#[test]
fn mismatched_reordered_and_stale_acknowledgments_fail_closed() {
    for line in [
        b"SEEDBOX_EFFECT id=2 action=stop guest_ns=1\n".as_slice(),
        b"SEEDBOX_EFFECT id=1 action=start guest_ns=1\n",
    ] {
        let mut c = Codec::default();
        command(&mut c, 1, ApplicationCommand::Stop);
        command(&mut c, 2, ApplicationCommand::Stop);
        assert_eq!(c.receive(&write(0, line)), Err(Error::Acknowledgment));
        assert_eq!(c.pending.len(), 2);
        assert_eq!(c.receive(&write(0, b"SBX_READY1\n")), Err(Error::Failed));
    }
    let mut c = Codec::default();
    assert_eq!(
        c.receive(&write(0, b"SEEDBOX_EFFECT id=1 action=start guest_ns=1\n")),
        Err(Error::Acknowledgment)
    );
}
#[test]
fn oracle_has_one_framing_owner_and_exact_reply_bytes() {
    let mut c = Codec::default();
    assert!(c
        .receive(&write(1, b"SBX_RPC1 1 1 {\"version\":1,"))
        .unwrap()
        .is_empty());
    let events = c
        .receive(&write(2, b"\"id\":1,\"op\":false}\nSBX_LOG1 1 61\n"))
        .unwrap();
    let Event::Oracle(r) = &events[0] else {
        panic!()
    };
    assert_eq!(r.observed, 2);
    assert_eq!(
        events[1],
        response(
            2,
            Response::ApplicationOutput {
                stream: Stream::Stdout,
                bytes: vec![b'a']
            }
        )
    );
    assert_eq!(
        c.oracle_reply(r.correlation, b"{\"error\":\"invalid op\"}")
            .unwrap(),
        Some(b"SBX_REPLY1 1 1 {\"error\":\"invalid op\"}\n".to_vec())
    );
    c.oracle_delivered(r.correlation).unwrap();
}
#[test]
fn confirmed_start_allows_next_outcome_but_rejected_start_does_not() {
    let mut c = Codec::default();
    c.receive(&write(1, b"SBX_EXIT1 status=23\n")).unwrap();
    command(&mut c, 1, ApplicationCommand::Start);
    c.receive(&write(2, b"SEEDBOX_FAILURE id=1 action=start guest_ns=2\n"))
        .unwrap();
    let mut rejected = c.clone();
    assert_eq!(
        rejected.receive(&write(3, b"SBX_EXIT1 status=23\n")),
        Err(Error::DuplicateOutcome)
    );
    command(&mut c, 2, ApplicationCommand::Start);
    c.receive(&write(
        3,
        b"SEEDBOX_EFFECT id=2 action=start guest_ns=3\nSBX_EXIT1 status=0\n",
    ))
    .unwrap();
    assert_eq!(
        c.receive(&write(4, b"SBX_EXIT1 status=0\n")),
        Err(Error::DuplicateOutcome)
    );
}

#[test]
fn initial_error_is_diagnostic_not_natural_outcome_and_console_is_separate() {
    let mut c = Codec::default();
    let events = c.receive(&write(4, b"SBX_ERROR1 errno=2\n")).unwrap();
    assert_eq!(
        events,
        [
            response(
                4,
                Response::BootConsole {
                    bytes: b"SBX_ERROR1 errno=2\n".to_vec()
                }
            ),
            Event::GuestError { time: 4, errno: 2 }
        ]
    );
    assert_eq!(
        Codec::console(5, b"SBX_READY1\n".to_vec()),
        response(
            5,
            Response::BootConsole {
                bytes: b"SBX_READY1\n".to_vec()
            }
        )
    );
    assert!(!c.ready);
    assert!(!c.outcome);
}
#[test]
fn terminal_error_preserves_diagnostic_and_suppresses_same_write_suffix() {
    let mut c = Codec::default();
    command(&mut c, 1, ApplicationCommand::Start);
    let events = c.receive(&write(4, b"SBX_LOG1 1 61\nSBX_ERROR1 errno=2\nSBX_READY1\nSEEDBOX_EFFECT id=1 action=start guest_ns=4\nSBX_EXIT1 status=0\nSBX_RPC1 1 1 {\"version\":1,\"id\":1}\n")).unwrap();
    assert_eq!(
        events,
        [
            response(
                4,
                Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes: vec![b'a']
                }
            ),
            response(
                4,
                Response::BootConsole {
                    bytes: b"SBX_ERROR1 errno=2\n".to_vec()
                }
            ),
            Event::GuestError { time: 4, errno: 2 },
        ]
    );
    assert!(!c.ready);
    assert!(!c.outcome);
    assert_eq!(c.pending.len(), 1);
    assert!(c.partial.is_empty());
    assert_eq!(c.receive(&write(5, b"SBX_READY1\n")), Err(Error::Failed));
}

#[test]
fn malformed_log_and_management_do_not_become_console() {
    for line in [
        b"SBX_LOG1 3 00\n".as_slice(),
        b"SBX_LOG1 1 0A\n",
        b"SBX_LOG1 1 f\n",
        b"SBX_EXIT1 status=256\n",
        b"kernel text\n",
        b"\0",
    ] {
        assert_eq!(Codec::default().receive(&write(0, line)), Err(Error::Frame));
    }
    let mut c = Codec::default();
    c.receive(&write(2, b"SBX_")).unwrap();
    assert_eq!(c.receive(&write(1, b"READY1\n")), Err(Error::Time));
}
#[test]
fn superseded_reply_is_nonfatal_through_codec_and_snapshot() {
    let mut codec = Codec::default();
    let events = codec
        .receive(&write(1, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}\n"))
        .unwrap();
    let Event::Oracle(old) = &events[0] else {
        panic!()
    };
    assert!(codec
        .oracle_reply(old.correlation, b"{}")
        .unwrap()
        .is_some());
    let events = codec
        .receive(&write(2, b"SBX_RPC1 2 2 {\"version\":1,\"id\":1}\n"))
        .unwrap();
    let Event::Oracle(new) = &events[0] else {
        panic!()
    };
    let mut restored: Codec = serde_json::from_slice(&serde_json::to_vec(&codec).unwrap()).unwrap();
    assert_eq!(restored.oracle_reply(old.correlation, b"{}"), Ok(None));
    assert_eq!(restored.oracle_delivered(old.correlation), Ok(false));
    assert!(restored
        .oracle_reply(new.correlation, b"{}")
        .unwrap()
        .is_some());
    assert_eq!(restored.oracle_delivered(new.correlation), Ok(true));
    assert!(restored
        .receive(&write(3, b"SBX_RPC1 3 2 {\"version\":1,\"id\":2}\n"))
        .is_ok());
}
#[test]
fn public_reply_identity_payload_and_duplicate_acceptance_fail_closed() {
    let prepare = || {
        let mut codec = Codec::default();
        codec
            .receive(&write(1, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}\n"))
            .unwrap();
        codec
    };
    let id = |session, request| super::super::SimulationControlId {
        session: NonZeroU32::new(session).unwrap(),
        request: NonZeroU32::new(request).unwrap(),
    };
    for bad in [id(1, 2), id(2, 1)] {
        assert!(prepare().prepare_reply(bad, b"{}").is_err());
    }
    for payload in [b"".as_slice(), b"{}\n", b"invalid", b"\0"] {
        assert!(prepare().prepare_reply(id(1, 1), payload).is_err());
    }
    let mut codec = prepare();
    assert!(codec.prepare_reply(id(1, 1), b"{}").unwrap().is_some());
    assert!(codec.prepare_reply(id(1, 1), b"{}").is_err());
    let mut codec = prepare();
    codec
        .receive(&write(2, b"SBX_RPC1 2 2 {\"version\":1,\"id\":1}\n"))
        .unwrap();
    assert!(codec.prepare_reply(id(1, 1), b"{}").unwrap().is_none());
    assert!(codec.prepare_reply(id(1, 1), b"invalid").is_err());
    let mut maximum = vec![b'a'; 16383];
    maximum[0] = b'"';
    maximum[16382] = b'"';
    assert!(prepare().prepare_reply(id(1, 1), &maximum).is_ok());
    maximum.insert(1, b'a');
    assert!(prepare().prepare_reply(id(1, 1), &maximum).is_err());
}
#[test]
fn realtime_steps_share_command_ids_and_acknowledge_as_step() {
    let mut c = Codec::default();
    let step = |n| Request::StepRealtime {
        id: id(1),
        nanoseconds: std::num::NonZeroI64::new(n).unwrap(),
    };
    assert_eq!(c.request(&step(-5)).unwrap(), b"step 1 -5\n");
    assert_eq!(c.request(&step(5)), Err(Error::ReusedId));
    assert_eq!(
        c.request(&Request::Application {
            id: id(1),
            command: ApplicationCommand::Start
        }),
        Err(Error::ReusedId)
    );
    assert_eq!(
        c.receive(&write(3, b"SEEDBOX_FAILURE id=1 action=step guest_ns=9\n"))
            .unwrap(),
        [Event::Response {
            time: 3,
            response: Response::CommandAcknowledged {
                id: id(1),
                result: CommandResult::Rejected
            }
        }]
    );
}
