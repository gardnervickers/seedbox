#[path = "../../seedbox-vmm/src/management_uart.rs"]
mod management_uart;
use seedbox_engine_core::{control, faults, links};
#[allow(dead_code)]
#[allow(clippy::manual_div_ceil)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/devices.rs"]
mod devices;
use serde_json::json;
fn names() -> Vec<String> {
    vec!["scenario".into(), "client".into(), "server".into()]
}
fn complete(c: &control::Control) -> bool {
    c.pending.is_none()
        && c.error.is_none()
        && c.log.iter().all(|row| {
            row["status"] != "cut_off"
                && row["delivered_at_ns"].as_u64() == row["response"]["replied_at_ns"].as_u64()
        })
}
fn control() -> control::Control {
    control::Control::new(&json!({"machine":"scenario","latency_ns":20,"placement":{"scenario":"scenario","client":"client","server":"server"}}),vec!["scenario".into(),"client".into(),"server".into()],10,5).unwrap()
}
fn request(
    c: &mut control::Control,
    id: u32,
    time: u64,
    body: serde_json::Value,
) -> Result<(), String> {
    c.request(
        0,
        time,
        format!("SBX_RPC1 {id} 1 {body}").as_bytes(),
        time + 15,
    )
}
#[test]
fn fixed_causal_admission_and_exact_reply_are_independent_of_progress() {
    let mut c = control();
    request(&mut c, 1, 100, json!({"version":1,"id":1,"op":"now"})).unwrap();
    assert_eq!(c.boundary(), 120);
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    c.apply_boundary(&mut f, 120).unwrap();
    assert!(c.response(139).is_err());
    // response refuses wrong timestamp without being used to fulfill any call.
    let mut c = control();
    request(&mut c, 1, 100, json!({"version":1,"id":1,"op":"now"})).unwrap();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    c.apply_boundary(&mut f, 120).unwrap();
    let bytes = c.response(140).unwrap();
    assert!(String::from_utf8(bytes).unwrap().contains("\"now_ns\":120"));
    assert!(!complete(&c));
    c.mark_delivered(140).unwrap();
    assert!(complete(&c));
    let mut c = control();
    assert!(c
        .request(
            0,
            100,
            b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}",
            121
        )
        .is_err());
}
#[test]
fn effects_must_finish_before_reply_and_overlapping_handles_survive_partial_heal() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    request(
        &mut c,
        1,
        100,
        json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, c.boundary()).unwrap();
    assert_eq!(f.apply_due(120), vec![(2, b"stop 1\n".to_vec())]);
    c.observe_faults(&f, 121).unwrap();
    assert!(c.pending.as_ref().unwrap().reply.is_none());
    f.effect(2, 1, 125, true);
    c.observe_faults(&f, 125).unwrap();
    assert_eq!(c.boundary(), 145);
    c.response(145).unwrap();
    c.mark_delivered(145).unwrap();
    for (relay, time, op) in [(2, 200, "partition"), (3, 300, "partition")] {
        request(&mut c,relay,time,json!({"version":1,"id":relay,"op":op,"left":[{"service":"client"}],"right":[{"service":"server"}]})).unwrap();
        c.apply_boundary(&mut f, c.boundary()).unwrap();
        f.apply_due(time + 20);
        c.observe_faults(&f, time + 20).unwrap();
        c.response(time + 40).unwrap();
        c.mark_delivered(time + 40).unwrap();
    }
    assert_eq!(f.active_handles(), ["p1", "p2"]);
    assert!(f.blocked(1, 2));
    request(
        &mut c,
        4,
        400,
        json!({"version":1,"id":4,"op":"heal","handle":"p1"}),
    )
    .unwrap();
    c.apply_boundary(&mut f, c.boundary()).unwrap();
    f.apply_due(420);
    assert!(f.blocked(1, 2));
    assert_eq!(f.active_handles(), ["p2"]);
}
#[test]
fn framing_authority_and_errors_fail_closed() {
    let mut c = control();
    assert!(c.request(1, 10, b"SBX_RPC1 1 1 {}", 20).is_err());
    assert!(request(&mut c, 1, 10, json!({"version":2,"id":1,"op":"now"})).is_err());
    request(
        &mut c,
        1,
        10,
        json!({"version":1,"id":1,"op":"stop","target":{"service":"scenario"}}),
    )
    .unwrap();
    assert!(c.pending.as_ref().unwrap().error.is_some());
    assert!(request(&mut c, 2, 10, json!({"version":1,"id":2,"op":"now"})).is_err());
    let mut c = control();
    request(&mut c,1,10,json!({"version":1,"id":1,"op":"partition","left":[{"service":"scenario"}],"right":[{"service":"server"}]})).unwrap();
    assert!(c.pending.as_ref().unwrap().error.is_some());
    let mut c = control();
    assert!(request(&mut c, 2, 10, json!({"version":1,"id":1,"op":"now"})).is_err());
    let mut c = control();
    assert!(c
        .request(0, 10, &vec![b'x'; control::MAX_FRAME + 1], 20)
        .is_err());
}

#[test]
fn sleep_admission_invalid_handle_and_public_ids_use_real_transitions() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    request(
        &mut c,
        1,
        100,
        json!({"version":1,"id":1,"op":"sleep","duration_ns":30}),
    )
    .unwrap();
    assert_eq!(c.boundary(), 120);
    c.apply_boundary(&mut f, 120).unwrap();
    assert_eq!(c.boundary(), 150);
    c.apply_boundary(&mut f, 150).unwrap();
    assert_eq!(c.boundary(), 170);
    c.apply_boundary(&mut f, 170).unwrap();
    c.mark_delivered(170).unwrap();
    assert!(complete(&c));
    assert!(request(&mut c, 2, 200, json!({"version":1,"id":1,"op":"now"})).is_err());
    request(
        &mut c,
        2,
        200,
        json!({"version":1,"id":2,"op":"heal","handle":"unknown"}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 220).unwrap();
    let bytes = c.apply_boundary(&mut f, 240).unwrap().unwrap();
    assert!(String::from_utf8(bytes).unwrap().contains("\"ok\":false"));
    c.mark_delivered(240).unwrap();
    assert!(complete(&c));
    assert!(f.log.is_empty());
    assert!(request(&mut c, 3, 300, json!({"version":1,"id":4,"op":"now"})).is_err());
}
#[test]
fn rejected_or_missing_real_process_effect_is_fatal() {
    for reject in [true, false] {
        let mut c = control();
        let mut f = faults::Faults::new(
            vec![],
            vec!["scenario".into(), "client".into(), "server".into()],
        )
        .unwrap();
        request(
            &mut c,
            1,
            100,
            json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}}),
        )
        .unwrap();
        c.apply_boundary(&mut f, 120).unwrap();
        f.apply_due(120);
        let at = if reject {
            f.effect(2, 1, 130, false);
            130
        } else {
            let deadline = f.next_boundary();
            f.apply_due(deadline);
            deadline
        };
        c.observe_faults(&f, at).unwrap();
        assert!(f.error.is_some());
        assert!(c.pending.as_ref().unwrap().fatal);
        assert!(!complete(&c));
        let reply = c.boundary();
        c.apply_boundary(&mut f, reply).unwrap();
        assert!(c.has_undelivered_reply());
        assert!(c.error.is_some());
        c.mark_delivered(reply).unwrap();
        assert!(!c.has_undelivered_reply());
        assert!(!complete(&c));
    }
}

#[test]
fn actual_uart_divisor_bytes_do_not_contaminate_committed_request_frames() {
    let mut uart = devices::Uart::default();
    let mut framer = control::ManagementFramer::default();
    management_uart::write(&mut framer, &mut uart, 3, 0x80).unwrap();
    // Base-register writes while DLAB is set never reach the RPC wire.
    for byte in b"SBX_RPC1 99 1 bad\n" {
        assert!(management_uart::write(&mut framer, &mut uart, 0, *byte)
            .unwrap()
            .is_none());
    }
    assert!(uart.out.is_empty());
    management_uart::write(&mut framer, &mut uart, 3, 3).unwrap();
    for byte in b"SEEDBOX_EFFECT id=1\n" {
        assert!(management_uart::write(&mut framer, &mut uart, 0, *byte)
            .unwrap()
            .is_none());
    }
    let line = b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}";
    for byte in line {
        assert!(management_uart::write(&mut framer, &mut uart, 0, *byte)
            .unwrap()
            .is_none());
    }
    assert_eq!(
        management_uart::write(&mut framer, &mut uart, 0, b'\n').unwrap(),
        Some(line.to_vec())
    );
}

#[test]
fn pending_sleep_admission_and_effect_at_world_cutoff_are_retained_without_error() {
    for before_admission in [true, false] {
        let mut c = control();
        let mut f = faults::Faults::new(
            vec![],
            vec!["scenario".into(), "client".into(), "server".into()],
        )
        .unwrap();
        request(
            &mut c,
            1,
            100,
            json!({"version":1,"id":1,"op":"sleep","duration_ns":2000}),
        )
        .unwrap();
        assert!(matches!(
            c.pending.as_ref().unwrap().operation,
            Some(seedbox_control::Operation::Sleep { duration_ns: 2000 })
        ));
        if !before_admission {
            c.apply_boundary(&mut f, 120).unwrap();
        }
        c.cut_off(&f, if before_admission { 110 } else { 1000 });
        assert!(c.error.is_none());
        assert!(!complete(&c));
        assert_eq!(c.log[0]["status"], "cut_off");
        assert_eq!(c.log[0]["admitted_done"], !before_admission);
        assert_eq!(c.log[0]["effect_at_ns"], 2120);
        assert!(c.log[0]["fault_id"].is_null());
    }
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    f.require_readiness();
    request(
        &mut c,
        1,
        100,
        json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 120).unwrap();
    assert!(f.apply_due(120).is_empty());
    c.cut_off(&f, 1000);
    assert!(c.error.is_none());
    assert!(f.error.is_none());
    assert_eq!(c.log[0]["fault_id"], 1);
    assert_eq!(c.log[0]["plan"]["deadline_ns"], 5_000_000_120_u64);
    assert_eq!(f.log[0]["status"], "requested");
}
#[test]
fn typed_argument_rejections_are_correlated_and_recoverable() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    request(
        &mut c,
        1,
        100,
        json!({"version":1,"id":1,"op":"now","unsupported":true}),
    )
    .unwrap();
    assert!(c.pending.as_ref().unwrap().error.is_some());
    c.apply_boundary(&mut f, 120).unwrap();
    c.apply_boundary(&mut f, 140).unwrap();
    c.mark_delivered(140).unwrap();
    assert!(complete(&c));
    assert!(f.log.is_empty());
}

#[test]
fn bulk_shared_memory_frames_preserve_fragments_and_multiple_messages() {
    let mut framer = control::ManagementFramer::default();
    assert!(framer
        .receive(b"SBX_RPC1 1 1 {\"version\":1,")
        .unwrap()
        .is_empty());
    let frames=framer.receive(b"\"id\":1,\"op\":\"now\"}\nSEEDBOX_EFFECT id=1\nSBX_RPC1 2 1 {\"version\":1,\"id\":2,\"op\":\"inspect\"}\n").unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}"
    );
    assert_eq!(
        frames[1],
        b"SBX_RPC1 2 1 {\"version\":1,\"id\":2,\"op\":\"inspect\"}"
    );
    let mut framer = control::ManagementFramer::default();
    assert!(framer
        .receive(&vec![b'x'; control::MAX_TRANSPORT_FRAME + 1])
        .is_err());
}

#[test]
fn process_lifecycle_rejections_are_admission_errors_without_new_faults() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    // Restart does not exec another process over the initially running application.
    request(
        &mut c,
        1,
        100,
        json!({"version":1,"id":1,"op":"restart","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 120).unwrap();
    c.apply_boundary(&mut f, 140).unwrap();
    c.mark_delivered(140).unwrap();
    assert_eq!(c.log[0]["response"]["error"]["code"], "invalid_state");
    assert!(f.log.is_empty());
    assert!(c.error.is_none());
    request(
        &mut c,
        2,
        200,
        json!({"version":1,"id":2,"op":"stop","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 220).unwrap();
    assert_eq!(f.apply_due(220).len(), 1);
    assert_eq!(f.process_state(2), "Running"); // State changes only after the actual reap ACK.
    f.effect(2, 1, 225, true);
    assert_eq!(f.process_state(2), "Stopped");
    c.observe_faults(&f, 225).unwrap();
    c.apply_boundary(&mut f, 245).unwrap();
    c.mark_delivered(245).unwrap();
    request(
        &mut c,
        3,
        300,
        json!({"version":1,"id":3,"op":"stop","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 320).unwrap();
    c.apply_boundary(&mut f, 340).unwrap();
    c.mark_delivered(340).unwrap();
    assert_eq!(c.log[2]["response"]["error"]["code"], "invalid_state");
    assert_eq!(f.log.len(), 1);
    request(
        &mut c,
        4,
        400,
        json!({"version":1,"id":4,"op":"restart","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 420).unwrap();
    assert_eq!(f.apply_due(420).len(), 1);
    f.effect(2, 2, 425, true);
    assert_eq!(f.process_state(2), "Running");
    c.observe_faults(&f, 425).unwrap();
    c.apply_boundary(&mut f, 445).unwrap();
    c.mark_delivered(445).unwrap();
    assert!(c.error.is_none());
    assert!(f.error.is_none());
}
#[test]
fn target_exit_between_request_and_admission_is_recoverable() {
    for op in ["stop", "restart"] {
        let mut c = control();
        let mut f = faults::Faults::new(
            vec![],
            vec!["scenario".into(), "client".into(), "server".into()],
        )
        .unwrap();
        request(
            &mut c,
            1,
            100,
            json!({"version":1,"id":1,"op":op,"target":{"service":"server"}}),
        )
        .unwrap();
        c.reject_unavailable(&["server".into()]);
        c.apply_boundary(&mut f, 120).unwrap();
        c.apply_boundary(&mut f, 140).unwrap();
        c.mark_delivered(140).unwrap();
        assert_eq!(c.log[0]["response"]["error"]["code"], "target_unavailable");
        assert!(f.log.is_empty());
        assert!(c.error.is_none());
    }
}
#[test]
fn oversized_inspection_is_a_bounded_recoverable_reply() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    request(&mut c, 1, 100, json!({"version":1,"id":1,"op":"inspect"})).unwrap();
    let counters = (0..12)
        .flat_map(|from| {
            (0..12).filter(move |&to|to!=from).map(move |to|(
        format!("chatty-machine-{from:02}->chatty-machine-{to:02}"),
        json!({"delivered":u64::MAX,"partition_dropped":u64::MAX,"receiver_dropped":u64::MAX,"loss_dropped":u64::MAX})
    ))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    c.pending.as_mut().unwrap().result = json!({"network_counters":counters});
    assert!(
        serde_json::to_vec(&c.pending.as_ref().unwrap().result)
            .unwrap()
            .len()
            > control::MAX_FRAME
    );
    c.apply_boundary(&mut f, 120).unwrap();
    let bytes = c.apply_boundary(&mut f, 140).unwrap().unwrap();
    let payload = bytes.splitn(4, |b| *b == b' ').nth(3).unwrap();
    assert!(payload.len() <= control::MAX_FRAME);
    assert_eq!(c.log[0]["response"]["error"]["code"], "result_too_large");
    c.mark_delivered(140).unwrap();
    assert!(c.error.is_none());
    request(&mut c, 2, 200, json!({"version":1,"id":2,"op":"now"})).unwrap();
    c.apply_boundary(&mut f, 220).unwrap();
    c.apply_boundary(&mut f, 240).unwrap();
    c.mark_delivered(240).unwrap();
    assert_eq!(c.log[1]["response"]["ok"], true);
}
#[test]
fn sessions_reset_public_ids_but_cannot_repeat_relays_or_old_sessions() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    for (relay, session, time) in [(1, 1, 100), (2, 3, 200)] {
        // Empty connection session2 sent no frame.
        c.request(
            0,
            time,
            format!("SBX_RPC1 {relay} {session} {{\"version\":1,\"id\":1,\"op\":\"now\"}}")
                .as_bytes(),
            time + 15,
        )
        .unwrap();
        c.apply_boundary(&mut f, time + 20).unwrap();
        let bytes = c.apply_boundary(&mut f, time + 40).unwrap().unwrap();
        c.mark_delivered(time + 40).unwrap();
        assert!(bytes.starts_with(format!("SBX_REPLY1 {relay} {session} ").as_bytes()));
        assert_eq!(c.log[(relay - 1) as usize]["session_id"], session);
    }
    for line in [
        "SBX_RPC1 3 1 {\"version\":1,\"id\":2,\"op\":\"now\"}",
        "SBX_RPC1 2 4 {\"version\":1,\"id\":1,\"op\":\"now\"}",
        "SBX_RPC1 3 3 {\"version\":1,\"id\":1,\"op\":\"now\"}",
    ] {
        assert!(c.request(0, 300, line.as_bytes(), 315).is_err());
    }
}
#[test]
fn target_exit_after_admission_is_recoverable_only_while_effect_is_outstanding() {
    let names = || vec!["scenario".into(), "client".into(), "server".into()];
    // A stop awaiting readiness, a dispatched stop, and a dispatched restart.
    for (op, ready) in [("stop", false), ("stop", true), ("restart", true)] {
        let mut c = control();
        let mut f = faults::Faults::new(vec![], names()).unwrap();
        let (mut id, mut t) = (1, 100);
        if op == "restart" {
            request(
                &mut c,
                1,
                t,
                json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}}),
            )
            .unwrap();
            c.apply_boundary(&mut f, 120).unwrap();
            f.apply_due(120);
            f.effect(2, 1, 125, true);
            c.observe_faults(&f, 125).unwrap();
            c.apply_boundary(&mut f, 145).unwrap();
            c.mark_delivered(145).unwrap();
            (id, t) = (2, 200);
        }
        if !ready {
            f.require_readiness();
        }
        request(
            &mut c,
            id,
            t,
            json!({"version":1,"id":id,"op":op,"target":{"service":"server"}}),
        )
        .unwrap();
        c.apply_boundary(&mut f, t + 20).unwrap();
        assert_eq!(f.apply_due(t + 20).len(), usize::from(ready));
        // Another machine's exit, or an unknown fault, changes nothing.
        f.target_exited(id, 1, t + 30);
        f.target_exited(id + 1, 2, t + 30);
        assert_eq!(f.outcome(id).unwrap().0, faults::Status::Waiting);
        f.target_exited(id, 2, t + 30);
        assert_eq!(
            f.outcome(id).unwrap().0,
            faults::Status::Unavailable(t + 30)
        );
        assert_eq!(f.next_boundary(), u64::MAX); // The deadline cannot fail it later.
        c.observe_faults(&f, t + 30).unwrap();
        assert!(c.apply_boundary(&mut f, t + 50).unwrap().is_some());
        c.mark_delivered(t + 50).unwrap();
        let row = &c.log[id as usize - 1];
        assert_eq!(row["fault_id"], id);
        assert_eq!(row["response"]["error"]["code"], "target_unavailable");
        assert_eq!(row["response"]["effect_at_ns"], t + 30);
        assert_eq!(f.log[id as usize - 1]["status"], "unavailable");
        assert!(complete(&c) && f.error.is_none());
    }
    // An exit after the acknowledgement, or after a guest failure, keeps its result.
    for success in [true, false] {
        let mut c = control();
        let mut f = faults::Faults::new(vec![], names()).unwrap();
        request(
            &mut c,
            1,
            100,
            json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}}),
        )
        .unwrap();
        c.apply_boundary(&mut f, 120).unwrap();
        f.apply_due(120);
        f.effect(2, 1, 125, success);
        f.target_exited(1, 2, 125);
        c.observe_faults(&f, 125).unwrap();
        c.apply_boundary(&mut f, 145).unwrap();
        let expected = if success {
            faults::Status::Applied(125)
        } else {
            faults::Status::Failed
        };
        assert_eq!(f.outcome(1).unwrap().0, expected);
        assert_eq!(c.log[0]["response"]["ok"], success);
        assert_eq!(c.error.is_some(), !success);
        if !success {
            assert_eq!(c.log[0]["response"]["error"]["code"], "effect_failed");
        }
    }
}

#[test]
fn shape_uses_partition_handle_counter_and_only_changes_links_at_admission() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    request(&mut c,1,100,json!({"version":1,"id":1,"op":"shape","from":[{"service":"client"}],"to":[{"service":"server"}],"latency_ns":30,"jitter_ns":5,"loss_ppm":500_000})).unwrap();
    assert!(f.active_handles().is_empty());
    c.apply_boundary(&mut f, 120).unwrap();
    assert!(f.active_handles().is_empty());
    f.apply_due(120);
    assert_eq!(f.active_handles(), vec!["p1"]);
    assert_eq!(f.links.policy((1, 2)).latency_ns, 30);
    assert_eq!(f.links.policy((2, 1)).latency_ns, 1);
    c.observe_faults(&f, 120).unwrap();
    let reply = c.response(140).unwrap();
    assert!(String::from_utf8(reply)
        .unwrap()
        .contains("\"handle\":\"p1\""));
    c.mark_delivered(140).unwrap();
    request(&mut c,2,200,json!({"version":1,"id":2,"op":"partition","left":[{"service":"client"}],"right":[{"service":"server"}]})).unwrap();
    c.apply_boundary(&mut f, 220).unwrap();
    f.apply_due(220);
    c.observe_faults(&f, 220).unwrap();
    c.response(240).unwrap();
    c.mark_delivered(240).unwrap();
    assert_eq!(f.active_handles(), vec!["p1", "p2"]);
    request(
        &mut c,
        3,
        300,
        json!({"version":1,"id":3,"op":"heal","handle":"p1"}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 320).unwrap();
    f.apply_due(320);
    c.observe_faults(&f, 320).unwrap();
    c.response(340).unwrap();
    c.mark_delivered(340).unwrap();
    assert_eq!(f.active_handles(), vec!["p2"]);
    assert!(f.blocked(1, 2));
    assert_eq!(f.links.policy((1, 2)).latency_ns, 1);
}

#[test]
fn invalid_shape_values_and_sides_receive_correlated_invalid_request_without_effects() {
    let good = json!({"version":1,"id":1,"op":"shape","from":[{"service":"client"}],"to":[{"service":"server"}],"latency_ns":10,"jitter_ns":0,"loss_ppm":0});
    for (key, value) in [
        ("latency_ns", json!(9)),
        ("jitter_ns", json!(10)),
        ("loss_ppm", json!(1_000_001)),
        ("latency_ns", json!(3_600_000_000_001u64)),
        ("from", json!([])),
        ("to", json!([{"service":"client"}])),
        ("from", json!([{"service":"client"},{"machine":"client"}])),
        ("from", json!([{"service":"scenario"}])),
    ] {
        let mut c = control();
        let mut call = good.clone();
        call[key] = value;
        request(&mut c, 1, 100, call).unwrap();
        let mut f = faults::Faults::new(
            vec![],
            vec!["scenario".into(), "client".into(), "server".into()],
        )
        .unwrap();
        c.apply_boundary(&mut f, 120).unwrap();
        let reply = String::from_utf8(c.response(140).unwrap()).unwrap();
        assert!(reply.contains("invalid_request"), "{reply}");
        assert!(f.active_handles().is_empty());
        assert!(f.log.is_empty());
    }
}

#[test]
fn connection_discovery_admission_race_and_heal_evidence_use_actual_packet_state() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    let mut syn = vec![0; 54];
    syn[12..14].copy_from_slice(&[8, 0]);
    syn[14] = 0x45;
    syn[17] = 40;
    syn[23] = 6;
    syn[26..30].copy_from_slice(&[10, 0, 0, 1]);
    syn[30..34].copy_from_slice(&[10, 0, 0, 2]);
    syn[34..36].copy_from_slice(&4000u16.to_be_bytes());
    syn[37] = 80;
    syn[41] = 7;
    syn[46] = 0x50;
    syn[47] = 2;
    request(&mut c,1,100,json!({"version":1,"id":1,"op":"connections","from":{"service":"client"},"to":{"service":"server"}})).unwrap();
    // SYN arrives after request observation but before admission.
    f.route(1, 2, 110, &syn, false);
    c.apply_boundary(&mut f, 120).unwrap();
    c.response(140).unwrap();
    c.mark_delivered(140).unwrap();
    let selected = c.log[0]["response"]["result"]["connections"][0].clone();
    assert_eq!(selected["src_port"], 4000);
    assert!(!f.connections.used);
    request(
        &mut c,
        2,
        200,
        json!({"version":1,"id":2,"op":"drop_connection","connection":selected}),
    )
    .unwrap();
    // Tuple changes before admission: reject, do not bind to replacement.
    syn[41] = 9;
    f.route(1, 2, 210, &syn, false);
    c.apply_boundary(&mut f, 220).unwrap();
    c.response(240).unwrap();
    c.mark_delivered(240).unwrap();
    assert_eq!(c.log[1]["response"]["error"]["code"], "invalid_state");
    assert!(f.log.is_empty());
    assert!(!f.connections.used);
    let replacement = f.connections("client", "server", 0)[0].clone();
    request(
        &mut c,
        3,
        300,
        json!({"version":1,"id":3,"op":"drop_connection","connection":replacement}),
    )
    .unwrap();
    assert!(f.active_handles().is_empty());
    c.apply_boundary(&mut f, 320).unwrap();
    f.apply_due(320);
    c.observe_faults(&f, 320).unwrap();
    c.response(340).unwrap();
    c.mark_delivered(340).unwrap();
    assert_eq!(f.route(1, 2, 350, &syn, false), links::Outcome::Lost);
    let handle = c.log[2]["response"]["result"]["handle"].clone();
    request(
        &mut c,
        4,
        400,
        json!({"version":1,"id":4,"op":"heal","handle":handle}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 420).unwrap();
    f.apply_due(420);
    c.observe_faults(&f, 420).unwrap();
    c.response(440).unwrap();
    c.mark_delivered(440).unwrap();
    assert_eq!(
        c.log[3]["response"]["result"],
        json!({"matched":1,"dropped":1})
    );
    assert!(f.active_handles().is_empty());
    assert!(matches!(
        f.route(1, 2, 450, &syn, false),
        links::Outcome::Deliver(_)
    ));
}

// Exercise the same admission -> fault -> observation -> reply sequence as the scheduler.
fn lifecycle_call(c: &mut control::Control, f: &mut faults::Faults, id: u32, op: &str) {
    let t = u64::from(id) * 100;
    request(
        c,
        id,
        t,
        json!({"version":1,"id":id,"op":op,"target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(f, t + 20).unwrap();
    assert!(f.apply_due(t + 20).is_empty());
    c.observe_faults(f, t + 20).unwrap();
    c.apply_boundary(f, t + 40).unwrap();
    c.mark_delivered(t + 40).unwrap();
    assert!(complete(c) && f.error.is_none());
}

#[test]
fn machine_lifecycle_plans_and_effects_use_admission_and_expose_states() {
    let mut c = control();
    let mut f = faults::Faults::new(vec![], names()).unwrap();
    for (i, (op, state)) in [
        ("pause", "Paused"),
        ("resume", "Running"),
        ("crash", "Down"),
        ("reboot", "Running"),
    ]
    .iter()
    .enumerate()
    {
        let id = i as u32 + 1;
        lifecycle_call(&mut c, &mut f, id, op);
        let plan = f.plan(id).unwrap();
        let expected = match *op {
            "pause" => seedbox_control::Action::Pause {
                machine: "server".into(),
            },
            "resume" => seedbox_control::Action::Resume {
                machine: "server".into(),
            },
            "crash" => seedbox_control::Action::Crash {
                machine: "server".into(),
            },
            "reboot" => seedbox_control::Action::Reboot {
                machine: "server".into(),
            },
            _ => unreachable!(),
        };
        assert_eq!(plan.action, expected);
        assert_eq!(f.process_state(2), *state);
        assert_eq!(
            f.outcome(id).unwrap().0,
            faults::Status::Applied(plan.at_ns)
        );
        let response = &c.log[i]["response"];
        assert_eq!(response["ok"], true);
        assert_eq!(response["effect_at_ns"], response["admitted_at_ns"]);
        assert_eq!(response["effect_at_ns"], plan.at_ns);
    }
}

#[test]
fn machine_lifecycle_controller_targets_are_rejected_for_services_and_machines() {
    for op in ["pause", "resume", "crash", "reboot"] {
        for target in [json!({"service":"scenario"}), json!({"machine":"scenario"})] {
            let mut c = control();
            let mut f = faults::Faults::new(vec![], names()).unwrap();
            request(
                &mut c,
                1,
                100,
                json!({"version":1,"id":1,"op":op,"target":target}),
            )
            .unwrap();
            c.apply_boundary(&mut f, 120).unwrap();
            c.apply_boundary(&mut f, 140).unwrap();
            c.mark_delivered(140).unwrap();
            assert_eq!(c.log[0]["response"]["error"]["code"], "invalid_request");
            assert!(c.log[0]["response"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("controller"));
            assert!(f.log.is_empty() && complete(&c));
        }
    }
}

#[test]
fn machine_lifecycle_checks_current_life_at_admission_and_errors_are_recoverable() {
    for (initial, op, message) in [
        (None, "resume", "target machine is not paused"),
        (None, "reboot", "target machine is not down"),
        (Some("pause"), "pause", "target machine is not running"),
        (Some("pause"), "reboot", "target machine is not down"),
        (Some("pause"), "stop", "target machine is paused"),
        (Some("pause"), "restart", "target machine is paused"),
        (Some("crash"), "pause", "target machine is not running"),
        (Some("crash"), "resume", "target machine is not paused"),
        (Some("crash"), "crash", "target machine is already down"),
    ] {
        let mut c = control();
        let mut f = faults::Faults::new(vec![], names()).unwrap();
        // The request is planned while Up. Change Life before its admission.
        request(
            &mut c,
            1,
            100,
            json!({"version":1,"id":1,"op":op,"target":{"machine":"server"}}),
        )
        .unwrap();
        if let Some(initial) = initial {
            let action = if initial == "pause" {
                seedbox_control::Action::Pause {
                    machine: "server".into(),
                }
            } else {
                seedbox_control::Action::Crash {
                    machine: "server".into(),
                }
            };
            f.append(seedbox_control::Plan {
                id: 0,
                at_ns: 110,
                deadline_ns: 110,
                action,
            })
            .unwrap();
            f.apply_due(110);
        }
        let count = f.log.len();
        c.apply_boundary(&mut f, 120).unwrap();
        c.apply_boundary(&mut f, 140).unwrap();
        c.mark_delivered(140).unwrap();
        assert_eq!(c.log[0]["response"]["error"]["code"], "invalid_state");
        assert_eq!(c.log[0]["response"]["error"]["message"], message);
        assert!(c.log[0]["fault_id"].is_null());
        assert_eq!(f.log.len(), count);
        request(&mut c, 2, 200, json!({"version":1,"id":2,"op":"now"})).unwrap();
        c.apply_boundary(&mut f, 220).unwrap();
        c.apply_boundary(&mut f, 240).unwrap();
        c.mark_delivered(240).unwrap();
        assert!(complete(&c) && f.error.is_none());
    }
}

#[test]
fn exited_machines_reject_pause_resume_but_allow_crash_and_reboot() {
    let mut c = control();
    let mut f = faults::Faults::new(vec![], names()).unwrap();
    for (i, op) in ["pause", "resume", "crash", "reboot"].iter().enumerate() {
        let id = i as u32 + 1;
        let t = u64::from(id) * 100;
        request(
            &mut c,
            id,
            t,
            json!({"version":1,"id":id,"op":op,"target":{"machine":"server"}}),
        )
        .unwrap();
        c.reject_unavailable(&["server".into()]);
        c.apply_boundary(&mut f, t + 20).unwrap();
        f.apply_due(t + 20);
        c.observe_faults(&f, t + 20).unwrap();
        c.apply_boundary(&mut f, t + 40).unwrap();
        c.mark_delivered(t + 40).unwrap();
        if matches!(*op, "pause" | "resume") {
            assert_eq!(c.log[i]["response"]["error"]["code"], "target_unavailable");
            assert_eq!(
                c.log[i]["response"]["error"]["message"],
                "target VM has exited; crash and reboot it instead"
            );
            assert!(c.log[i]["fault_id"].is_null());
        } else {
            assert_eq!(c.log[i]["response"]["ok"], true);
        }
    }
    assert_eq!(f.log.len(), 2);
    assert!(complete(&c) && f.error.is_none());
}

#[test]
fn pending_process_fault_resolves_at_crash_and_down_process_faults_are_unavailable() {
    for ready in [true, false] {
        let mut c = control();
        let mut f = faults::Faults::new(vec![], names()).unwrap();
        if !ready {
            f.require_readiness();
        }
        request(
            &mut c,
            1,
            100,
            json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}}),
        )
        .unwrap();
        c.apply_boundary(&mut f, 120).unwrap();
        f.apply_due(120);
        f.append(seedbox_control::Plan {
            id: 0,
            at_ns: 130,
            deadline_ns: 130,
            action: seedbox_control::Action::Crash {
                machine: "server".into(),
            },
        })
        .unwrap();
        f.apply_due(130);
        c.observe_faults(&f, 999).unwrap(); // Resolution uses the crash, not observation time.
        c.apply_boundary(&mut f, 150).unwrap();
        c.mark_delivered(150).unwrap();
        assert_eq!(c.log[0]["response"]["error"]["code"], "target_unavailable");
        assert_eq!(c.log[0]["response"]["effect_at_ns"], 130);
        assert_eq!(f.outcome(1).unwrap().0, faults::Status::Unavailable(130));
        for (id, op) in [(2, "stop"), (3, "restart")] {
            lifecycle_call(&mut c, &mut f, id, op);
            assert_eq!(
                c.log[id as usize - 1]["response"]["error"]["code"],
                "target_unavailable"
            );
            assert_eq!(
                c.log[id as usize - 1]["response"]["effect_at_ns"],
                u64::from(id) * 100 + 20
            );
        }
    }
}

#[test]
fn matched_stop_trigger_rejects_target_already_stopped_through_control() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    request(
        &mut c,
        1,
        100,
        json!({"version":1,"id":1,"op":"arm_trigger","source":{"service":"server"},"marker":[65],"match_timeout_ns":1000,"fault":{"action":"stop","target":{"service":"server"}}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 120).unwrap();
    c.apply_boundary(&mut f, 140).unwrap();
    c.mark_delivered(140).unwrap();
    assert_eq!(c.triggers.records[0].status, "armed");
    request(
        &mut c,
        2,
        200,
        json!({"version":1,"id":2,"op":"stop","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 220).unwrap();
    assert_eq!(f.apply_due(220), vec![(2, b"stop 1\n".to_vec())]);
    f.effect(2, 1, 225, true);
    c.observe_faults(&f, 225).unwrap();
    c.apply_boundary(&mut f, 245).unwrap();
    c.mark_delivered(245).unwrap();
    assert_eq!(f.process_state(2), "Stopped");
    let stopped_log = f.log.clone();

    c.triggers.committed("server", 260, b'A', 20, 275).unwrap();
    assert_eq!(c.triggers.records[0].status, "matched");
    c.triggers.apply_boundary(&mut f, 280).unwrap();
    let record = &c.triggers.records[0];
    assert_eq!(record.status, "rejected");
    assert_eq!(
        record.rejection_reason.as_deref(),
        Some("target process is already stopped")
    );
    assert_eq!(record.matched_at_ns, Some(260));
    assert_eq!(record.effect_at_ns, Some(280));
    assert_eq!(record.fault_id, None);
    let evidence = c.triggers.evidence().unwrap().unwrap();
    assert_eq!(
        evidence[0]["rejection_reason"],
        "target process is already stopped"
    );
    assert_eq!(c.triggers.boundary(), u64::MAX);
    c.triggers.apply_boundary(&mut f, 280).unwrap();
    c.triggers.committed("server", 281, b'A', 20, 296).unwrap();
    assert!(f.apply_due(280).is_empty());
    assert_eq!(f.log, stopped_log);
    assert_eq!(f.next_boundary(), u64::MAX);
    assert_eq!(f.process_state(2), "Stopped");
    assert!(f.error.is_none());
    assert!(complete(&c));
}

#[test]
fn arm_cancel_use_admission_and_match_is_not_the_arm_reply_effect() {
    let mut c = control();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    request(&mut c,1,100,json!({"version":1,"id":1,"op":"arm_trigger","source":{"service":"server"},"marker":[65,66],"match_timeout_ns":1000,"fault":{"action":"partition","left":[{"service":"server"}],"right":[{"service":"client"}]}})).unwrap();
    assert!(!c.triggers.observes("server"));
    c.apply_boundary(&mut f, 120).unwrap();
    assert!(c.triggers.observes("server"));
    assert!(f.log.is_empty());
    c.apply_boundary(&mut f, 140).unwrap();
    c.mark_delivered(140).unwrap();
    assert_eq!(
        c.log[0]["response"]["result"],
        json!({"trigger_id":1,"handle":"p1"})
    );
    c.triggers.committed("server", 150, 65, 20, 160).unwrap();
    request(
        &mut c,
        2,
        151,
        json!({"version":1,"id":2,"op":"cancel_trigger","trigger_id":1}),
    )
    .unwrap();
    assert!(c.triggers.observes("server"));
    c.apply_boundary(&mut f, 171).unwrap();
    c.apply_boundary(&mut f, 191).unwrap();
    c.mark_delivered(191).unwrap();
    assert!(!c.triggers.observes("server"));
    assert_eq!(c.triggers.records[0].cancelled_at_ns, Some(171));
    assert!(f.log.is_empty());
}

#[test]
fn arm_rejections_and_pending_arm_cutoff_remain_correlated() {
    for change in [
        json!({"marker":[]}),
        json!({"marker":vec![0;257]}),
        json!({"match_timeout_ns":0}),
        json!({"source":{"machine":"unknown"}}),
        json!({"fault":{"action":"stop","target":{"service":"scenario"}}}),
    ] {
        let mut c = control();
        let mut f = faults::Faults::new(
            vec![],
            vec!["scenario".into(), "client".into(), "server".into()],
        )
        .unwrap();
        let mut body = json!({"version":1,"id":1,"op":"arm_trigger","source":{"service":"server"},"marker":[65],"match_timeout_ns":1000,"fault":{"action":"stop","target":{"service":"server"}}});
        body.as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        request(&mut c, 1, 100, body).unwrap();
        c.apply_boundary(&mut f, 120).unwrap();
        c.apply_boundary(&mut f, 140).unwrap();
        c.mark_delivered(140).unwrap();
        assert_eq!(c.log[0]["response"]["ok"], false);
        assert!(c.triggers.evidence().unwrap().is_none());
        assert!(f.log.is_empty());
    }
    for admitted in [false, true] {
        let mut c = control();
        let mut f = faults::Faults::new(
            vec![],
            vec!["scenario".into(), "client".into(), "server".into()],
        )
        .unwrap();
        request(&mut c,1,100,json!({"version":1,"id":1,"op":"arm_trigger","source":{"service":"server"},"marker":[65],"match_timeout_ns":1000,"fault":{"action":"stop","target":{"service":"server"}}})).unwrap();
        if admitted {
            c.apply_boundary(&mut f, 120).unwrap();
        }
        c.cut_off(&f, if admitted { 130 } else { 110 });
        assert_eq!(c.triggers.records.len(), usize::from(admitted));
        if admitted {
            assert_eq!(c.log[0]["result"]["trigger_id"], 1);
        }
    }
}

#[test]
fn unsupported_operations_reply_without_planning_handles_or_faults() {
    let operations = [
        json!({"op":"connections","from":{"service":"client"},"to":{"service":"server"}}),
        json!({"op":"drop_connection","connection":{"from":"client","to":"server","src_addr":"10.0.0.1","dst_addr":"10.0.0.2","src_port":1000,"dst_port":2000,"incarnation":1}}),
        json!({"op":"arm_trigger","source":{"service":"client"},"marker":[65],"match_timeout_ns":100,"fault":{"action":"partition","left":[{"service":"client"}],"right":[{"service":"server"}]}}),
        json!({"op":"cancel_trigger","trigger_id":1}),
    ];
    for mut body in operations {
        let mut c = control();
        let mut f = faults::Faults::new(vec![], names()).unwrap();
        body["version"] = 1.into();
        body["id"] = 1.into();
        c.request_with_support(
            0,
            100,
            format!("SBX_RPC1 1 1 {body}").as_bytes(),
            115,
            |_| false,
        )
        .unwrap();
        let pending = c.pending.as_ref().unwrap();
        assert!(pending.plan.is_none());
        assert_eq!(pending.result, json!({}));
        assert_eq!(pending.error_code, Some("unsupported"));
        assert!(c.triggers.records.is_empty());
        assert!(c.apply_boundary(&mut f, 120).unwrap().is_none());
        let bytes = c.apply_boundary(&mut f, 140).unwrap().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let reply: serde_json::Value =
            serde_json::from_str(text.splitn(4, ' ').nth(3).unwrap()).unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "unsupported");
        assert!(reply.get("result").is_none());
        c.mark_delivered(140).unwrap();
        assert!(complete(&c));
        assert!(f.log.is_empty());
        // A rejected trigger/selector must not consume the next ordinary handle.
        request(&mut c, 2, 150, json!({"version":1,"id":2,"op":"partition","left":[{"service":"client"}],"right":[{"service":"server"}]})).unwrap();
        let pending = c.pending.as_ref().unwrap();
        assert_eq!(pending.result["handle"], "p1");
    }
}
