#[path = "../../vmm/src/control.rs"]
mod control;
#[allow(dead_code)]
#[allow(clippy::manual_div_ceil)]
#[path = "../../vmm/src/devices.rs"]
mod devices;
#[allow(dead_code)]
#[path = "../../vmm/src/faults.rs"]
mod faults;
use serde_json::json;
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
        format!("SBX_RPC1 {id} {body}").as_bytes(),
        1000,
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
    c.apply_boundary(&mut f, 120, 1000).unwrap();
    assert!(c.response(139).is_err());
    // response refuses wrong timestamp without being used to fulfill any call.
    let mut c = control();
    request(&mut c, 1, 100, json!({"version":1,"id":1,"op":"now"})).unwrap();
    let mut f = faults::Faults::new(
        vec![],
        vec!["scenario".into(), "client".into(), "server".into()],
    )
    .unwrap();
    c.apply_boundary(&mut f, 120, 1000).unwrap();
    let bytes = c.response(140).unwrap();
    assert!(String::from_utf8(bytes).unwrap().contains("\"now_ns\":120"));
    assert!(!c.complete());
    c.log[0]["delivered_at_ns"] = json!(140);
    assert!(c.complete());
    let mut c = control();
    assert!(c
        .request(
            0,
            100,
            b"SBX_RPC1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}",
            1000,
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
    c.apply_boundary(&mut f, c.boundary(), 1000).unwrap();
    assert_eq!(f.apply_due(120), vec![(2, b"stop 1\n".to_vec())]);
    c.observe_faults(&f, 121, 1000).unwrap();
    assert!(c.pending.as_ref().unwrap().reply.is_none());
    f.effect(2, 1, 125, true);
    c.observe_faults(&f, 125, 1000).unwrap();
    assert_eq!(c.boundary(), 145);
    c.response(145).unwrap();
    c.log[0]["delivered_at_ns"] = json!(145);
    for (relay, time, op) in [(2, 200, "partition"), (3, 300, "partition")] {
        request(&mut c,relay,time,json!({"version":1,"id":relay,"op":op,"left":[{"service":"client"}],"right":[{"service":"server"}]})).unwrap();
        c.apply_boundary(&mut f, c.boundary(), 1000).unwrap();
        f.apply_due(time + 20);
        c.observe_faults(&f, time + 20, 1000).unwrap();
        c.response(time + 40).unwrap();
        c.log.last_mut().unwrap()["delivered_at_ns"] = json!(time + 40);
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
    c.apply_boundary(&mut f, c.boundary(), 1000).unwrap();
    f.apply_due(420);
    assert!(f.blocked(1, 2));
    assert_eq!(f.active_handles(), ["p2"]);
}
#[test]
fn framing_authority_and_errors_fail_closed() {
    let mut c = control();
    assert!(c.request(1, 10, b"SBX_RPC1 1 {}", 1000, 20).is_err());
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
        .request(0, 10, &vec![b'x'; control::MAX_FRAME + 1], 1000, 20)
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
    c.apply_boundary(&mut f, 120, 1000).unwrap();
    assert_eq!(c.boundary(), 150);
    c.apply_boundary(&mut f, 150, 1000).unwrap();
    assert_eq!(c.boundary(), 170);
    c.apply_boundary(&mut f, 170, 1000).unwrap();
    c.log[0]["delivered_at_ns"] = json!(170);
    assert!(c.complete());
    assert!(request(&mut c, 2, 200, json!({"version":1,"id":1,"op":"now"})).is_err());
    request(
        &mut c,
        2,
        200,
        json!({"version":1,"id":2,"op":"heal","handle":"unknown"}),
    )
    .unwrap();
    c.apply_boundary(&mut f, 220, 1000).unwrap();
    let bytes = c.apply_boundary(&mut f, 240, 1000).unwrap().unwrap();
    assert!(String::from_utf8(bytes).unwrap().contains("\"ok\":false"));
    c.log[1]["delivered_at_ns"] = json!(240);
    assert!(c.complete());
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
        c.apply_boundary(&mut f, 120, 1000).unwrap();
        f.apply_due(120);
        let at = if reject {
            f.effect(2, 1, 130, false);
            130
        } else {
            let deadline = f.next_boundary();
            f.apply_due(deadline);
            deadline
        };
        c.observe_faults(&f, at, 1000).unwrap();
        assert!(f.error.is_some());
        assert!(c.pending.as_ref().unwrap().fatal);
        assert!(!c.complete());
        let reply = c.boundary();
        c.apply_boundary(&mut f, reply, 1000).unwrap();
        assert!(c.has_undelivered_reply());
        assert!(c.error.is_some());
        c.log[0]["delivered_at_ns"] = json!(reply);
        assert!(!c.has_undelivered_reply());
        assert!(!c.complete());
    }
}

#[test]
fn actual_uart_divisor_bytes_do_not_contaminate_committed_request_frames() {
    let mut uart = devices::Uart::default();
    let mut framer = control::ManagementFramer::default();
    framer.write(&mut uart, 3, 0x80).unwrap();
    // Base-register writes while DLAB is set never reach the RPC wire.
    for byte in b"SBX_RPC1 99 bad\n" {
        assert!(framer.write(&mut uart, 0, *byte).unwrap().is_none());
    }
    assert!(uart.out.is_empty());
    framer.write(&mut uart, 3, 3).unwrap();
    for byte in b"SEEDBOX_EFFECT id=1\n" {
        assert!(framer.write(&mut uart, 0, *byte).unwrap().is_none());
    }
    let line = b"SBX_RPC1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}";
    for byte in line {
        assert!(framer.write(&mut uart, 0, *byte).unwrap().is_none());
    }
    assert_eq!(
        framer.write(&mut uart, 0, b'\n').unwrap(),
        Some(line.to_vec())
    );
}
