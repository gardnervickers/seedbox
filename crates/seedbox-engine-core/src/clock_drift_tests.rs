use super::*;
use crate::control::Control;
use seedbox_control::MAX_CLOCK_RATE_PPB;

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
fn plan(id: u32, at: u64, action: &str, handle: &str, machine: &str, extra: Value) -> Value {
    let mut plan = json!({"id":id,"at_ns":at,"deadline_ns":at + 100,"action":action,"handle":handle,"machine":machine});
    plan.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    plan
}
fn drift(id: u32, at: u64, handle: &str, machine: &str, rate: Value) -> Value {
    plan(id, at, "clock_drift", handle, machine, rate)
}
fn heal(id: u32, at: u64, handle: &str) -> Value {
    json!({"id":id,"at_ns":at,"deadline_ns":at + 100,"action":"heal","handle":handle})
}
fn crash(id: u32, at: u64, machine: &str) -> Value {
    json!({"id":id,"at_ns":at,"deadline_ns":at,"action":"crash","machine":machine})
}
fn detail(f: &Faults, row: usize) -> &str {
    f.log[row]["detail"].as_str().unwrap()
}

#[test]
fn fixed_drift_sets_an_absolute_rate_and_heal_restores_nominal() {
    let mut f = seeded(
        7,
        vec![
            drift(1, 10, "d", "b", json!({"rate_ppb":-250_000})),
            heal(2, 20, "d"),
        ],
    );
    assert_eq!(
        f.apply_due(10),
        vec![(1, b"clock_rate 1 -250000\n".to_vec())]
    );
    assert_eq!(f.active_handles(), ["d"]);
    assert_eq!(
        f.admission_error(&Action::Heal { handle: "d".into() }),
        Some("clock drift is not settled")
    );
    f.effect(1, 1, 12, true);
    assert_eq!(f.outcome(1).unwrap().0, Status::Applied(12));
    assert_eq!(f.log[0]["rate_ppb"], -250_000);
    assert!(f.log[0].get("offset_ns").is_none());
    assert!(detail(&f, 0).contains("rate set to -250000 ppb"));
    assert_eq!(f.apply_due(20), vec![(1, b"clock_rate 2 0\n".to_vec())]);
    f.effect(1, 2, 25, true);
    assert_eq!(f.outcome(2).unwrap().0, Status::Applied(25));
    assert_eq!(f.log[1]["rate_ppb"], 0);
    assert!(f.active_handles().is_empty());
    assert!(f.error.is_none() && f.rejected_effects.is_empty());
}

#[test]
fn seeded_drift_shares_the_clock_stream_and_draws_only_when_used() {
    let max = 10_000_000u64;
    let values = |seed: u64, plans: Vec<Value>| {
        let mut f = seeded(seed, plans);
        f.apply_due(100);
        f.log
            .iter()
            .map(|row| {
                row.get("rate_ppb")
                    .or(row.get("offset_ns"))
                    .unwrap()
                    .as_i64()
                    .unwrap()
            })
            .collect::<Vec<_>>()
    };
    let first = values(1, vec![drift(1, 10, "d", "b", json!({"max_rate_ppb":max}))]);
    assert!(first[0].unsigned_abs() <= max);
    assert_eq!(
        first,
        values(1, vec![drift(1, 10, "d", "b", json!({"max_rate_ppb":max}))])
    );
    assert_ne!(
        first,
        values(2, vec![drift(1, 10, "d", "b", json!({"max_rate_ppb":max}))])
    );
    // The drift draws the same stream position a seeded skew would: a fixed
    // drift and fixed skew draw nothing, a seeded skew first takes one draw.
    let skew_first = values(
        1,
        vec![plan(
            1,
            10,
            "clock_skew",
            "k",
            "b",
            json!({"max_offset_ns":max}),
        )],
    );
    assert_eq!(skew_first, first);
    let mixed = values(
        1,
        vec![
            drift(1, 10, "f", "a", json!({"rate_ppb":5})),
            plan(2, 10, "clock_skew", "s", "b", json!({"offset_ns":3})),
            plan(3, 10, "clock_skew", "k", "b", json!({"max_offset_ns":max})),
            drift(4, 10, "d", "b", json!({"max_rate_ppb":max})),
        ],
    );
    assert_eq!(&mixed[..3], &[5, 3, first[0]]);
    let mut stream = seeded(1, vec![]);
    stream.links.clock_offset(1, max).unwrap();
    assert_eq!(mixed[3], stream.links.clock_offset(1, max).unwrap());
}

#[test]
fn one_drift_per_machine_until_healed_or_crashed() {
    let action = |handle: &str, machine: &str| Action::ClockDrift {
        handle: handle.into(),
        machine: machine.into(),
        rate_ppb: Some(1),
        max_rate_ppb: None,
    };
    let mut f = seeded(
        0,
        vec![
            drift(1, 10, "d", "b", json!({"rate_ppb":1})),
            drift(2, 10, "e", "b", json!({"rate_ppb":2})),
            crash(3, 20, "b"),
            heal(4, 30, "d"),
        ],
    );
    assert_eq!(f.apply_due(10), vec![(1, b"clock_rate 1 1\n".to_vec())]);
    // The second drift on the same machine is refused at admission and fails
    // if it was planned anyway; a skew or another machine's drift is fine.
    assert_eq!(
        f.admission_error(&action("x", "b")),
        Some("target machine clock is already drifting")
    );
    assert_eq!(f.admission_error(&action("x", "a")), None);
    assert_eq!(f.outcome(2).unwrap().0, Status::Failed);
    f.error = None;
    f.effect(1, 1, 11, true);
    assert_eq!(
        f.admission_error(&action("x", "b")),
        Some("target machine clock is already drifting")
    );
    // The new incarnation runs at the nominal rate, so the machine may drift
    // again and healing the old handle contacts no guest.
    f.apply_due(20);
    assert_eq!(
        f.admission_error(&action("x", "b")),
        Some("target machine is down")
    );
    assert!(f.apply_due(30).is_empty());
    assert_eq!(f.outcome(4).unwrap().0, Status::Applied(30));
    assert!(detail(&f, 3).contains("drift ended with its machine incarnation"));
    assert!(f.active_handles().is_empty());
}

#[test]
fn a_lost_drift_does_not_block_a_new_one() {
    let mut f = seeded(
        0,
        vec![
            drift(1, 10, "d", "b", json!({"rate_ppb":1})),
            crash(2, 20, "b"),
            json!({"id":3,"at_ns":30,"deadline_ns":30,"action":"reboot","machine":"b"}),
        ],
    );
    f.apply_due(10);
    f.effect(1, 1, 11, true);
    f.apply_due(30);
    assert_eq!(f.active_handles(), ["d"]);
    let again = Action::ClockDrift {
        handle: "e".into(),
        machine: "b".into(),
        rate_ppb: Some(2),
        max_rate_ppb: None,
    };
    assert_eq!(f.admission_error(&again), None);
}

#[test]
fn drift_admission_and_bounds() {
    let action = |machine: &str| Action::ClockDrift {
        handle: "d".into(),
        machine: machine.into(),
        rate_ppb: Some(1),
        max_rate_ppb: None,
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
    for (rate, max) in [
        (Some(0), None),
        (None, Some(0)),
        (Some(1), Some(1)),
        (None, None),
        (Some(-(MAX_CLOCK_RATE_PPB as i64) - 1), None),
        (None, Some(MAX_CLOCK_RATE_PPB + 1)),
    ] {
        let mut row = drift(1, 10, "d", "a", json!({}));
        if let Some(r) = rate {
            row["rate_ppb"] = json!(r);
        }
        if let Some(m) = max {
            row["max_rate_ppb"] = json!(m);
        }
        let mut f = seeded(0, vec![]);
        assert!(f.append(serde_json::from_value(row).unwrap()).is_err());
    }
    let mut f = seeded(
        3,
        vec![drift(
            1,
            10,
            "d",
            "a",
            json!({"max_rate_ppb":MAX_CLOCK_RATE_PPB}),
        )],
    );
    f.apply_due(10);
    assert!(f.log[0]["rate_ppb"].as_i64().unwrap().unsigned_abs() <= MAX_CLOCK_RATE_PPB);
    // A guest that refuses the rate fails the fault and frees the machine.
    let mut f = seeded(0, vec![drift(1, 10, "d", "a", json!({"rate_ppb":-1}))]);
    f.apply_due(10);
    f.effect(0, 1, 11, false);
    assert_eq!(f.outcome(1).unwrap().0, Status::Failed);
    assert!(f.active_handles().is_empty());
}

#[test]
fn control_returns_handle_then_rate_and_heal_waits_for_the_guest() {
    let mut c = Control::new(
        &json!({"machine":"a","latency_ns":20,"placement":{"svc":"b"}}),
        names(),
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
        json!({"version":1,"id":1,"op":"clock_drift","target":{"service":"svc"},"max_rate_ppb":1000}),
    );
    let rate = f.log[0]["rate_ppb"].as_i64().unwrap();
    assert_eq!(
        commands,
        vec![(1, format!("clock_rate 1 {rate}\n").into_bytes())]
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
    assert_eq!(reply["result"], json!({"handle":"p1","rate_ppb":rate}));
    assert_eq!(reply["effect_at_ns"], 130);
    let commands = call(
        &mut c,
        &mut f,
        2,
        200,
        json!({"version":1,"id":2,"op":"heal","handle":"p1"}),
    );
    assert_eq!(commands, vec![(1, b"clock_rate 2 0\n".to_vec())]);
    f.effect(1, 2, 230, true);
    c.observe_faults(&f, 230).unwrap();
    assert!(c.response(250).is_ok());
    c.mark_delivered(250).unwrap();
    assert!(f.active_handles().is_empty() && c.error.is_none());
    call(
        &mut c,
        &mut f,
        3,
        300,
        json!({"version":1,"id":3,"op":"clock_drift","target":{"machine":"b"},"rate_ppb":0}),
    );
    assert!(String::from_utf8(c.response(340).unwrap())
        .unwrap()
        .contains("invalid_request"));
    assert_eq!(f.log.len(), 2);
}
