use super::*;
use crate::{
    clock::SimulationClock,
    pv::{ApplicationCommand, CommandResult, Inbox, Input, Outbox, Request, Response},
    rng::SeededRng,
};
use std::{collections::VecDeque, num::NonZeroU32};

#[test]
fn natural_outcome_is_published_once_without_caching_a_machine_exit() {
    let mut bridge = GuestBridge::default();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(1);
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    let mut context = RunContext {
        execution: Default::default(),
        worker: &mut (),
        clock: &clock,
        rng: &mut rng,
        inbox: Inbox::new(&mut inbox, Timestamp::from_nanos(0)),
        outbox: Outbox::new(
            &mut outputs,
            Timestamp::from_nanos(0),
            Timestamp::from_nanos(9),
        ),
    };
    bridge
        .observe_ordered(
            &mut context,
            vec![GuestWrite::Lifecycle(LifecycleWrite {
                time: 3,
                bytes: b"SBX_EXIT1 status=23\n".to_vec(),
            })],
        )
        .unwrap();
    bridge.observe_ordered(&mut context, Vec::new()).unwrap();
    let mut restored: GuestBridge =
        serde_json::from_slice(&serde_json::to_vec(&bridge).unwrap()).unwrap();
    assert_eq!(outputs.len(), 1);
    assert!(matches!(
        outputs[0].payload,
        Response::ApplicationOutcome { code: 23 }
    ));
    restored.reset();
    restored.reset();
}

#[test]
fn ordered_console_and_fragmented_logs_keep_actual_times_and_ties() {
    let mut bridge = GuestBridge::default();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(1);
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    let mut context = RunContext {
        execution: Default::default(),
        worker: &mut (),
        clock: &clock,
        rng: &mut rng,
        inbox: Inbox::new(&mut inbox, Timestamp::from_nanos(0)),
        outbox: Outbox::new(
            &mut outputs,
            Timestamp::from_nanos(0),
            Timestamp::from_nanos(20),
        ),
    };
    let write = |time, bytes: &[u8]| LifecycleWrite {
        time,
        bytes: bytes.to_vec(),
    };
    bridge
        .observe_ordered(
            &mut context,
            vec![
                GuestWrite::Console(write(1, b"A")),
                GuestWrite::Lifecycle(write(1, b"SBX_LOG1 1 42\nSBX_LOG1 2 ")),
                GuestWrite::Console(write(1, b"C")),
                GuestWrite::Lifecycle(write(5, b"44\n")),
                GuestWrite::Console(write(5, b"E")),
            ],
        )
        .unwrap();
    assert_eq!(
        outputs
            .iter()
            .map(|output| output.at.as_nanos())
            .collect::<Vec<_>>(),
        [1, 1, 1, 5, 5]
    );
    let bytes: Vec<_> = outputs
        .iter()
        .flat_map(|output| match &output.payload {
            Response::BootConsole { bytes } | Response::ApplicationOutput { bytes, .. } => {
                bytes.clone()
            }
            _ => panic!("unexpected response"),
        })
        .collect();
    assert_eq!(bytes, b"ABCDE");
    assert!(matches!(
        outputs[3].payload,
        Response::ApplicationOutput {
            stream: super::super::Stream::Stderr,
            ..
        }
    ));
}

#[test]
fn accepted_command_and_fragment_survive_snapshot_without_early_output() {
    let id = NonZeroU32::new(7).unwrap();
    let mut inbox = VecDeque::from([Input {
        at: Timestamp::from_nanos(0),
        payload: Request::Application {
            id,
            command: ApplicationCommand::Pause,
        },
    }]);
    let mut outputs = Vec::new();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(1);
    let mut worker = ();
    let mut bridge = GuestBridge::default();
    {
        let mut context = RunContext {
            execution: Default::default(),
            worker: &mut worker,
            clock: &clock,
            rng: &mut rng,
            inbox: Inbox::new(&mut inbox, Timestamp::from_nanos(0)),
            outbox: Outbox::new(
                &mut outputs,
                Timestamp::from_nanos(0),
                Timestamp::from_nanos(10),
            ),
        };
        assert_eq!(
            bridge
                .request(&context.inbox.front().unwrap().payload)
                .unwrap(),
            b"freeze 7\n"
        );
        context.inbox.take();
        bridge
            .observe(
                &mut context,
                vec![LifecycleWrite {
                    time: 5,
                    bytes: b"SEEDBOX_EFFECT id=7 action=freeze guest_ns=".to_vec(),
                }],
                5,
                Vec::new(),
            )
            .unwrap();
    }
    assert!(inbox.is_empty());
    assert!(outputs.is_empty());
    let snapshot = serde_json::to_vec(&bridge).unwrap();
    let mut restored: GuestBridge = serde_json::from_slice(&snapshot).unwrap();
    let mut context = RunContext {
        execution: Default::default(),
        worker: &mut worker,
        clock: &clock,
        rng: &mut rng,
        inbox: Inbox::new(&mut inbox, Timestamp::from_nanos(10)),
        outbox: Outbox::new(
            &mut outputs,
            Timestamp::from_nanos(10),
            Timestamp::from_nanos(20),
        ),
    };
    assert!(context.inbox.front().is_none());
    restored
        .observe(
            &mut context,
            vec![LifecycleWrite {
                time: 12,
                bytes: b"5\n".to_vec(),
            }],
            12,
            Vec::new(),
        )
        .unwrap();
    assert_eq!(
        outputs,
        [Output {
            at: Timestamp::from_nanos(12),
            payload: Response::CommandAcknowledged {
                id,
                result: CommandResult::Completed
            }
        }]
    );
}
#[test]
fn reconnect_publishes_both_requests_and_prepares_only_current_reply() {
    let mut bridge = GuestBridge::default();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(1);
    let mut worker = ();
    let mut inputs = VecDeque::new();
    let mut outputs = Vec::new();
    let mut context = RunContext {
        execution: Default::default(),
        worker: &mut worker,
        clock: &clock,
        rng: &mut rng,
        inbox: Inbox::new(&mut inputs, Timestamp::from_nanos(2)),
        outbox: Outbox::new(
            &mut outputs,
            Timestamp::from_nanos(0),
            Timestamp::from_nanos(2),
        ),
    };
    for (time, bytes) in [
        (1, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}\n".as_slice()),
        (2, b"SBX_RPC1 2 2 {\"version\":1,\"id\":1}\n".as_slice()),
    ] {
        bridge
            .observe(
                &mut context,
                vec![LifecycleWrite {
                    time,
                    bytes: bytes.to_vec(),
                }],
                time,
                Vec::new(),
            )
            .unwrap();
    }
    let mut restored: GuestBridge =
        serde_json::from_slice(&serde_json::to_vec(&bridge).unwrap()).unwrap();
    assert_eq!(outputs.len(), 2);
    let Response::SimulationControl { id: old, .. } = outputs[0].payload.clone() else {
        panic!()
    };
    let Response::SimulationControl { id: new, .. } = outputs[1].payload.clone() else {
        panic!()
    };
    assert!(restored
        .request(&Request::SimulationReply {
            id: old,
            payload: b"{}".to_vec()
        })
        .unwrap()
        .is_empty());
    assert!(!restored
        .request(&Request::SimulationReply {
            id: new,
            payload: b"{}".to_vec()
        })
        .unwrap()
        .is_empty());
    assert_eq!(restored.prepared_replies.len(), 1);
    restored.input_delivered().unwrap();
    assert!(restored.prepared_replies.is_empty());
    assert_eq!(outputs[0].at, Timestamp::from_nanos(1));
    assert_eq!(outputs[1].at, Timestamp::from_nanos(2));
}
#[test]
fn final_fragment_timestamp_and_prepared_reply_survive_snapshot_until_fifo_transfer() {
    let mut bridge = GuestBridge::default();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(1);
    let mut worker = ();
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    let mut context = RunContext {
        execution: Default::default(),
        worker: &mut worker,
        clock: &clock,
        rng: &mut rng,
        inbox: Inbox::new(&mut inbox, Timestamp::from_nanos(9)),
        outbox: Outbox::new(
            &mut outputs,
            Timestamp::from_nanos(0),
            Timestamp::from_nanos(9),
        ),
    };
    bridge
        .observe(
            &mut context,
            vec![LifecycleWrite {
                time: 2,
                bytes: b"SBX_RPC1 1 1 {\"version\":1,".to_vec(),
            }],
            2,
            Vec::new(),
        )
        .unwrap();
    bridge
        .observe(
            &mut context,
            vec![LifecycleWrite {
                time: 9,
                bytes: b"\"id\":1}\n".to_vec(),
            }],
            9,
            Vec::new(),
        )
        .unwrap();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].at, Timestamp::from_nanos(9));
    let Response::SimulationControl { id, payload } = &outputs[0].payload else {
        panic!()
    };
    assert_eq!(payload, b"{\"version\":1,\"id\":1}");
    let queued = bridge
        .request(&Request::SimulationReply {
            id: *id,
            payload: b"{}".to_vec(),
        })
        .unwrap();
    assert_eq!(queued, b"SBX_REPLY1 1 1 {}\n");
    // The machine captures this bridge together with its queued RX bytes.
    let image = serde_json::to_vec(&(bridge, queued)).unwrap();
    let (mut restored, queued): (GuestBridge, Vec<u8>) = serde_json::from_slice(&image).unwrap();
    assert_eq!(queued, b"SBX_REPLY1 1 1 {}\n");
    assert_eq!(restored.prepared_replies.len(), 1);
    let mut premature = restored.clone();
    assert!(premature
        .codec
        .receive(&LifecycleWrite {
            time: 10,
            bytes: b"SBX_RPC1 2 1 {\"version\":1,\"id\":2}\n".to_vec()
        })
        .is_err());
    restored.input_delivered().unwrap();
    restored.input_delivered().unwrap(); // Empty batches do not acknowledge twice.
    assert!(restored
        .codec
        .receive(&LifecycleWrite {
            time: 10,
            bytes: b"SBX_RPC1 2 1 {\"version\":1,\"id\":2}\n".to_vec()
        })
        .is_ok());
}
fn call(
    bridge: &mut GuestBridge,
    relay: u32,
    wire_session: u32,
    request: u32,
) -> Result<super::super::SimulationControlId, String> {
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(1);
    let mut inputs = VecDeque::new();
    let mut outputs = Vec::new();
    let result = bridge.observe(
        &mut RunContext {
            execution: Default::default(),
            worker: &mut (),
            clock: &clock,
            rng: &mut rng,
            inbox: Inbox::new(&mut inputs, Timestamp::from_nanos(0)),
            outbox: Outbox::new(
                &mut outputs,
                Timestamp::from_nanos(0),
                Timestamp::from_nanos(0),
            ),
        },
        vec![LifecycleWrite {
            time: 0,
            bytes: format!("SBX_RPC1 {relay} {wire_session} {{\"version\":1,\"id\":{request}}}\n")
                .into_bytes(),
        }],
        0,
        Vec::new(),
    );
    if let Err(error) = result {
        assert!(outputs.is_empty());
        return Err(error);
    }
    assert_eq!(outputs.len(), 1);
    let Response::SimulationControl { id, .. } = outputs[0].payload else {
        panic!()
    };
    Ok(id)
}
fn reply(
    bridge: &mut GuestBridge,
    id: super::super::SimulationControlId,
) -> Result<Vec<u8>, String> {
    bridge.request(&Request::SimulationReply {
        id,
        payload: b"{}".to_vec(),
    })
}

#[test]
fn public_sessions_are_lazy_and_never_reused_by_boot_resets() {
    let mut bridge = GuestBridge::default();
    bridge.reset();
    bridge.reset();
    assert_eq!(bridge.session_highwater, 0);
    let first = call(&mut bridge, 1, 7, 1).unwrap();
    assert_eq!(first.session.get(), 1);
    assert_eq!(reply(&mut bridge, first).unwrap(), b"SBX_REPLY1 1 7 {}\n");
    bridge.input_delivered().unwrap();
    let second = call(&mut bridge, 2, 7, 2).unwrap();
    assert_eq!(second.session, first.session);
    let replaced = call(&mut bridge, 3, 9, 1).unwrap();
    assert_eq!(replaced.session.get(), 2);
    assert!(reply(&mut bridge, first).unwrap().is_empty());
    bridge.reset();
    bridge.reset();
    assert_eq!(bridge.session_highwater, 2);
    assert!(reply(&mut bridge, replaced).unwrap().is_empty());
    let mut invalid = bridge.clone();
    assert!(reply(
        &mut invalid,
        super::super::SimulationControlId {
            session: NonZeroU32::new(3).unwrap(),
            request: NonZeroU32::new(1).unwrap()
        }
    )
    .is_err());
    let rebooted = call(&mut bridge, 1, 1, 1).unwrap();
    assert_eq!(rebooted.session.get(), 3);
    assert!(reply(&mut bridge, replaced).unwrap().is_empty());
    assert_eq!(
        reply(&mut bridge, rebooted).unwrap(),
        b"SBX_REPLY1 1 1 {}\n"
    );
}

#[test]
fn snapshot_preserves_public_mapping_prepared_wire_keys_and_reset_highwater() {
    let mut bridge = GuestBridge::default();
    let first = call(&mut bridge, 1, 7, 1).unwrap();
    let queued = reply(&mut bridge, first).unwrap();
    let image = serde_json::to_vec(&(bridge.clone(), queued)).unwrap();
    bridge.reset();
    assert!(bridge.prepared_replies.is_empty());
    assert!(reply(&mut bridge, first).unwrap().is_empty());
    let (mut restored, queued): (GuestBridge, Vec<u8>) = serde_json::from_slice(&image).unwrap();
    assert_eq!(queued, b"SBX_REPLY1 1 7 {}\n");
    assert_eq!(restored.prepared_replies[0].session.get(), 7);
    assert_eq!(restored.session.unwrap().public.get(), 1);
    restored.input_delivered().unwrap();
    let new = call(&mut restored, 2, 8, 1).unwrap();
    assert_eq!(new.session.get(), 2);
    restored.reset();
    let mut restored: GuestBridge =
        serde_json::from_slice(&serde_json::to_vec(&restored).unwrap()).unwrap();
    assert!(reply(&mut restored, first).unwrap().is_empty());
    assert!(reply(&mut restored, new).unwrap().is_empty());
    assert_eq!(call(&mut restored, 1, 1, 1).unwrap().session.get(), 3);
}

#[test]
fn session_exhaustion_fails_before_wrap_or_alias_but_existing_session_continues() {
    let mut bridge = GuestBridge {
        session_highwater: u32::MAX - 1,
        ..Default::default()
    };
    let last = call(&mut bridge, 1, 11, 1).unwrap();
    assert_eq!(last.session.get(), u32::MAX);
    reply(&mut bridge, last).unwrap();
    bridge.input_delivered().unwrap();
    let next_request = call(&mut bridge, 2, 11, 2).unwrap();
    assert_eq!(next_request.session, last.session);
    bridge.reset();
    assert!(reply(&mut bridge, last).unwrap().is_empty());
    assert_eq!(
        call(&mut bridge, 1, 1, 1).unwrap_err(),
        "simulation-control session IDs exhausted"
    );
    assert_eq!(bridge.session_highwater, u32::MAX);
    assert!(bridge.session.is_none());
}

#[test]
fn retired_reply_framing_and_current_identity_still_fail_closed() {
    let mut bridge = GuestBridge::default();
    let first = call(&mut bridge, 1, 1, 1).unwrap();
    bridge.reset();
    let current = call(&mut bridge, 1, 1, 1).unwrap();
    assert_ne!(first.session, current.session);
    assert_eq!(current.session.get(), 2);
    assert_eq!(
        reply(&mut bridge.clone(), current).unwrap(),
        b"SBX_REPLY1 1 1 {}\n"
    );
    let mut malformed = bridge.clone();
    assert!(malformed
        .request(&Request::SimulationReply {
            id: first,
            payload: b"invalid".to_vec()
        })
        .is_err());
    let mut wrong = current;
    wrong.request = NonZeroU32::new(2).unwrap();
    assert!(reply(&mut bridge, wrong).is_err());
}
