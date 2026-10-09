use super::*;
use crate::control::Control;
use seedbox_control::MAX_CLOCK_OFFSET_NS;

fn names() -> Vec<String> {
    vec!["a".into(), "b".into(), "c".into()]
}
fn seeded(seed: u64, plans: Vec<Value>) -> Faults {
    let links = Links::new(
        seed,
        &names(),
        Policy {
            latency_ns: 10,
            jitter_ns: 0,
            loss_ppm: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
        },
        BTreeMap::new(),
    )
    .unwrap();
    Faults::with_links(plans, names(), links).unwrap()
}
fn skew(id: u32, at: u64, handle: &str, machine: &str, offset: Value) -> Value {
    let mut plan = json!({"id":id,"at_ns":at,"deadline_ns":at + 100,"action":"clock_skew","handle":handle,"machine":machine});
    plan.as_object_mut()
        .unwrap()
        .extend(offset.as_object().unwrap().clone());
    plan
}
fn heal(id: u32, at: u64, handle: &str) -> Value {
    json!({"id":id,"at_ns":at,"deadline_ns":at + 100,"action":"heal","handle":handle})
}
fn crash(id: u32, at: u64, machine: &str) -> Value {
    json!({"id":id,"at_ns":at,"deadline_ns":at,"action":"crash","machine":machine})
}

#[test]
fn fixed_skew_steps_the_guest_and_heal_steps_it_back() {
    let mut f = seeded(
        7,
        vec![
            skew(1, 10, "k", "b", json!({"offset_ns":-1_500_000_001i64})),
            heal(2, 20, "k"),
        ],
    );
    assert_eq!(
        f.apply_due(10),
        vec![(1, b"clock_step 1 -1500000001\n".to_vec())]
    );
    assert_eq!(f.active_handles(), ["k"]);
    assert_eq!(f.outcome(1).unwrap().0, Status::Waiting);
    // The admission check refuses healing an unacknowledged skew.
    assert_eq!(
        f.admission_error(&Action::Heal { handle: "k".into() }),
        Some("clock skew is not settled")
    );
    f.effect(1, 1, 12, true);
    assert_eq!(f.outcome(1).unwrap().0, Status::Applied(12));
    assert_eq!(f.log[0]["offset_ns"], -1_500_000_001i64);
    assert!(f.log[0]["detail"]
        .as_str()
        .unwrap()
        .contains("stepped by -1500000001 ns"));
    assert_eq!(
        f.apply_due(20),
        vec![(1, b"clock_step 2 1500000001\n".to_vec())]
    );
    assert_eq!(f.active_handles(), ["k"]);
    f.effect(1, 2, 25, true);
    assert_eq!(f.outcome(2).unwrap().0, Status::Applied(25));
    assert_eq!(f.log[1]["offset_ns"], 1_500_000_001i64);
    assert!(f.active_handles().is_empty());
    assert!(f.error.is_none() && f.rejected_effects.is_empty());
}

#[test]
fn seeded_skews_draw_per_machine_streams_within_bounds_only_when_used() {
    let max = 1_000_000_000u64;
    let draw = |seed: u64, plans: Vec<Value>| {
        let mut f = seeded(seed, plans);
        let commands = f.apply_due(100);
        let offsets: Vec<i64> = f
            .log
            .iter()
            .filter(|row| row["action"] == "clock_skew")
            .map(|row| row["offset_ns"].as_i64().unwrap())
            .collect();
        (commands.len(), offsets)
    };
    let both = |seed| {
        draw(
            seed,
            vec![
                skew(1, 10, "x", "a", json!({"max_offset_ns":max})),
                skew(2, 10, "y", "b", json!({"max_offset_ns":max})),
            ],
        )
    };
    let (sent, first) = both(1);
    assert_eq!(sent, 2);
    assert_eq!(first, both(1).1, "same seed, same offsets");
    assert!(first.iter().all(|o| o.unsigned_abs() <= max));
    assert_ne!(first[0], first[1], "machines draw from separate streams");
    assert_ne!(first, both(2).1, "seeds differ");
    // A fixed skew draws nothing: a later seeded skew on the same machine sees
    // the stream's first value. A seeded skew on another machine does not
    // advance this machine's stream either.
    let alone = draw(1, vec![skew(1, 10, "y", "b", json!({"max_offset_ns":max}))]).1;
    let after = draw(
        1,
        vec![
            skew(1, 10, "f", "b", json!({"offset_ns":5})),
            skew(2, 10, "x", "a", json!({"max_offset_ns":max})),
            skew(3, 10, "y", "b", json!({"max_offset_ns":max})),
        ],
    )
    .1;
    assert_eq!(alone[0], first[1]);
    assert_eq!(after, vec![5, first[0], first[1]]);
}

#[test]
fn dispatch_waits_for_readiness_and_ack_keeps_process_state() {
    let mut f = seeded(
        0,
        vec![
            json!({"id":1,"at_ns":5,"deadline_ns":105,"action":"stop","machine":"b"}),
            skew(2, 10, "k", "b", json!({"offset_ns":1})),
        ],
    );
    f.require_readiness();
    assert!(f.apply_due(10).is_empty());
    f.management_ready(1, 30, 4).unwrap();
    assert_eq!(
        f.apply_due(34),
        vec![(1, b"stop 1\n".to_vec()), (1, b"clock_step 2 1\n".to_vec())]
    );
    f.effect(1, 1, 40, true);
    f.effect(1, 2, 41, true);
    // Acknowledging a clock step must not restart a stopped process's state.
    assert_eq!(f.process_state(1), "Stopped");
    assert_eq!(f.outcome(2).unwrap().0, Status::Applied(41));
}

#[test]
fn crash_resolves_pending_skews_and_ends_active_ones() {
    // Pending at the crash: unavailable, and its handle is gone.
    let mut f = seeded(
        0,
        vec![
            skew(1, 10, "k", "b", json!({"offset_ns":1})),
            crash(2, 20, "b"),
        ],
    );
    f.apply_due(20);
    assert_eq!(f.outcome(1).unwrap().0, Status::Unavailable(20));
    assert!(f.active_handles().is_empty());
    // Active at the crash: the new incarnation boots from the recorded wall
    // clock, so a heal sends nothing and succeeds at once.
    let mut f = seeded(
        0,
        vec![
            skew(1, 10, "k", "b", json!({"offset_ns":1})),
            crash(2, 20, "b"),
            heal(3, 30, "k"),
        ],
    );
    f.apply_due(10);
    f.effect(1, 1, 11, true);
    f.apply_due(20);
    assert_eq!(f.active_handles(), ["k"]);
    assert!(f.apply_due(30).is_empty());
    assert_eq!(f.outcome(3).unwrap().0, Status::Applied(30));
    assert!(f.log[2]["detail"]
        .as_str()
        .unwrap()
        .contains("ended with its machine incarnation"));
    assert!(f.active_handles().is_empty() && f.error.is_none());
}

#[test]
fn rejections_fail_and_admission_follows_machine_life() {
    let mut f = seeded(0, vec![skew(1, 10, "k", "b", json!({"offset_ns":-1}))]);
    f.apply_due(10);
    f.effect(1, 1, 11, false);
    assert_eq!(f.outcome(1).unwrap().0, Status::Failed);
    assert!(f.active_handles().is_empty() && f.error.is_some());

    let action = |machine: &str| Action::ClockSkew {
        handle: "k".into(),
        machine: machine.into(),
        offset_ns: Some(1),
        max_offset_ns: None,
    };
    let mut f = seeded(
        0,
        vec![
            json!({"id":1,"at_ns":10,"deadline_ns":10,"action":"pause","machine":"b"}),
            crash(2, 10, "c"),
        ],
    );
    f.apply_due(10);
    assert_eq!(f.admission_error(&action("a")), None);
    assert_eq!(
        f.admission_error(&action("b")),
        Some("target machine is paused")
    );
    assert_eq!(
        f.admission_error(&action("c")),
        Some("target machine is down")
    );

    for (offset, max) in [
        (Some(0), None),
        (None, Some(0)),
        (Some(1), Some(1)),
        (None, None),
        (Some(-(MAX_CLOCK_OFFSET_NS as i64) - 1), None),
        (None, Some(MAX_CLOCK_OFFSET_NS + 1)),
    ] {
        let mut plan = json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"clock_skew","handle":"k","machine":"a"});
        if let Some(o) = offset {
            plan["offset_ns"] = json!(o);
        }
        if let Some(m) = max {
            plan["max_offset_ns"] = json!(m);
        }
        let mut f = seeded(0, vec![]);
        assert!(f.append(serde_json::from_value(plan).unwrap()).is_err());
    }
    // The largest seeded bound cannot overflow its draw.
    let mut f = seeded(
        3,
        vec![skew(
            1,
            10,
            "k",
            "a",
            json!({"max_offset_ns":MAX_CLOCK_OFFSET_NS}),
        )],
    );
    f.apply_due(10);
    assert!(f.log[0]["offset_ns"].as_i64().unwrap().unsigned_abs() <= MAX_CLOCK_OFFSET_NS);
}

#[test]
fn control_returns_handle_then_offset_and_heal_waits_for_the_guest() {
    let names = names();
    let mut c = Control::new(
        &json!({"machine":"a","latency_ns":20,"placement":{"svc":"b"}}),
        names.clone(),
        10,
        5,
    )
    .unwrap();
    let mut f = seeded(9, vec![]);
    fn call(
        c: &mut Control,
        f: &mut Faults,
        relay: u32,
        at: u64,
        body: Value,
    ) -> Vec<(usize, Vec<u8>)> {
        c.request(
            0,
            at,
            format!("SBX_RPC1 {relay} 1 {body}").as_bytes(),
            at + 15,
        )
        .unwrap();
        c.apply_boundary(f, at + 20).unwrap();
        let commands = f.apply_due(at + 20);
        c.observe_faults(f, at + 20).unwrap();
        commands
    }
    let commands = call(
        &mut c,
        &mut f,
        1,
        100,
        json!({"version":1,"id":1,"op":"clock_skew","target":{"service":"svc"},"max_offset_ns":1000}),
    );
    assert_eq!(commands.len(), 1);
    let offset = f.log[0]["offset_ns"].as_i64().unwrap();
    assert_eq!(
        commands[0],
        (1, format!("clock_step 1 {offset}\n").into_bytes())
    );
    assert!(c.pending.as_ref().unwrap().reply.is_none());
    f.effect(1, 1, 130, true);
    c.observe_faults(&f, 130).unwrap();
    let reply: Value = serde_json::from_slice(
        c.response(150)
            .unwrap()
            .strip_prefix(b"SBX_REPLY1 1 1 ")
            .unwrap(),
    )
    .unwrap();
    c.mark_delivered(150).unwrap();
    assert_eq!(reply["result"], json!({"handle":"p1","offset_ns":offset}));
    assert_eq!(reply["effect_at_ns"], 130);
    let commands = call(
        &mut c,
        &mut f,
        2,
        200,
        json!({"version":1,"id":2,"op":"heal","handle":"p1"}),
    );
    assert_eq!(
        commands,
        vec![(1, format!("clock_step 2 {}\n", -offset).into_bytes())]
    );
    f.effect(1, 2, 230, true);
    c.observe_faults(&f, 230).unwrap();
    assert!(c.response(250).is_ok());
    c.mark_delivered(250).unwrap();
    assert!(f.active_handles().is_empty() && c.error.is_none());
    // Invalid arguments are a correlated request error with no fault.
    call(
        &mut c,
        &mut f,
        3,
        300,
        json!({"version":1,"id":3,"op":"clock_skew","target":{"machine":"b"},"offset_ns":1,"max_offset_ns":1}),
    );
    let reply = c.response(340).unwrap();
    assert!(String::from_utf8(reply)
        .unwrap()
        .contains("invalid_request"));
    assert_eq!(f.log.len(), 2);
}
