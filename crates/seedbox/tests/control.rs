#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/control.rs"]
mod control;
#[allow(dead_code)]
#[allow(clippy::manual_div_ceil)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/devices.rs"]
mod devices;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/faults.rs"]
mod faults;
use serde_json::json;
fn complete(c: &control::Control) -> bool {
    c.pending.is_none()
        && c.error.is_none()
        && c.log.iter().all(|row| {
            row["status"] != "cut_off"
                && row["delivered_at_ns"].as_u64() == row["response"]["replied_at_ns"].as_u64()
        })
}
fn control() -> control::Control {
    control::Control::new(&json!({"machine":"scenario","service":"scenario","latency_ns":20,"placement":{"scenario":"scenario","client":"client","server":"server"}}),vec!["scenario".into(),"client".into(),"server".into()],10,5).unwrap()
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
    assert_eq!(f.apply_due(120), vec![(2, b"stop 1 server\n".to_vec())]);
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
    framer.write(&mut uart, 3, 0x80).unwrap();
    // Base-register writes while DLAB is set never reach the RPC wire.
    for byte in b"SBX_RPC1 99 1 bad\n" {
        assert!(framer.write(&mut uart, 0, *byte).unwrap().is_none());
    }
    assert!(uart.out.is_empty());
    framer.write(&mut uart, 3, 3).unwrap();
    for byte in b"SEEDBOX_EFFECT id=1\n" {
        assert!(framer.write(&mut uart, 0, *byte).unwrap().is_none());
    }
    let line = b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}";
    for byte in line {
        assert!(framer.write(&mut uart, 0, *byte).unwrap().is_none());
    }
    assert_eq!(
        framer.write(&mut uart, 0, b'\n').unwrap(),
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
    let frames=framer.receive(b"\"id\":1,\"op\":\"now\"}\nSEEDBOX_EFFECT id=1\nSBX_RPC1 2 1 {\"version\":1,\"id\":2,\"op\":\"inspect\"}\nSBX_EXIT1 server 3\n").unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[2], b"SBX_EXIT1 server 3");
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
    assert_eq!(f.process_state("server"), "Running"); // State changes only after the actual reap ACK.
    f.effect(2, 1, 225, true);
    assert_eq!(f.process_state("server"), "Stopped");
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
    assert_eq!(f.process_state("server"), "Running");
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
        json!({"delivered":u64::MAX,"partition_dropped":u64::MAX,"receiver_dropped":u64::MAX})
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

fn shared_control() -> control::Control {
    control::Control::new(&json!({"machine":"kernel","service":"scenario","latency_ns":20,"placement":{"scenario":"kernel","client":"kernel","server":"kernel"}}),vec!["kernel".into()],10,5).unwrap()
}
fn shared_faults() -> faults::Faults {
    let placement = ["scenario", "client", "server"]
        .into_iter()
        .map(|s| (s.to_owned(), "kernel".to_owned()))
        .collect();
    faults::Faults::with_placement(vec![], vec!["kernel".into()], &placement).unwrap()
}
#[test]
fn shared_kernel_services_beside_the_controller_remain_fault_targets() {
    let mut c = shared_control();
    let mut f = shared_faults();
    request(&mut c,1,100,json!({"version":1,"id":1,"op":"partition","left":[{"service":"client"}],"right":[{"service":"server"}]})).unwrap();
    assert!(c.pending.as_ref().unwrap().error.is_none());
    c.apply_boundary(&mut f, 120).unwrap();
    assert_eq!(
        f.apply_due(120),
        vec![(0, b"partition 1 p1 client server\n".to_vec())]
    );
    c.observe_faults(&f, 121).unwrap();
    assert!(c.pending.as_ref().unwrap().reply.is_none());
    f.effect(0, 1, 130, true);
    c.observe_faults(&f, 130).unwrap();
    assert_eq!(c.boundary(), 150);
    let reply = String::from_utf8(c.response(150).unwrap()).unwrap();
    assert!(reply.contains("\"handle\":\"p1\"") && reply.contains("\"effect_at_ns\":130"));
    c.mark_delivered(150).unwrap();
    request(
        &mut c,
        2,
        200,
        json!({"version":1,"id":2,"op":"stop","target":{"service":"server"}}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 220).unwrap();
    assert_eq!(f.apply_due(220), vec![(0, b"stop 2 server\n".to_vec())]);
    // The machine hosts the controller and other services: name a service.
    for (id, target) in [
        (3, json!({"machine":"kernel"})),
        (3, json!({"service":"scenario"})),
    ] {
        let mut c = shared_control();
        request(
            &mut c,
            id - 2,
            300,
            json!({"version":1,"id":id-2,"op":"stop","target":target}),
        )
        .unwrap();
        assert!(c
            .pending
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("controller"));
    }
}
#[test]
fn control_service_must_be_placed_on_the_control_machine() {
    for config in [
        json!({"machine":"kernel","latency_ns":20,"placement":{"scenario":"kernel"}}),
        json!({"machine":"kernel","service":"other","latency_ns":20,"placement":{"scenario":"kernel"}}),
        json!({"machine":"kernel","service":"scenario","latency_ns":20,"placement":{"scenario":"elsewhere"}}),
    ] {
        assert!(control::Control::new(&config, vec!["kernel".into()], 10, 5).is_err());
    }
}
