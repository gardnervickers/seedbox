use seedbox_engine_core::{faults as engine, world_events as events};
use serde_json::json;
#[test]
fn already_routed_frames_survive_same_time_partition_and_events_keep_sequence() {
    use std::collections::BTreeMap;
    let mut f = engine::Faults::new(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"cut","left":["n1"],"right":["n2"]}),
        json!({"id":2,"at_ns":11,"deadline_ns":21,"action":"heal","handle":"cut"}),
    ],vec!["n1".into(),"n2".into()]).unwrap();
    // These frames passed the production send-time partition gate before10.
    assert!(!f.blocked(0, 1));
    let mut pending = BTreeMap::from([
        ((10, 2), b"second".as_slice()),
        ((10, 1), b"first".as_slice()),
        ((12, 3), b"after".as_slice()),
    ]);
    assert!(events::pop_due(&mut pending, 9).is_none());
    f.apply_due(10);
    assert!(f.blocked(0, 1)); // New same-time sends are dropped after the fault.
    assert_eq!(events::pop_due(&mut pending, 10), Some(b"first".as_slice()));
    assert_eq!(
        events::pop_due(&mut pending, 10),
        Some(b"second".as_slice())
    );
    assert!(events::pop_due(&mut pending, 10).is_none());
    f.apply_due(11);
    assert!(!f.blocked(0, 1));
    assert_eq!(
        events::pop_due_at(&mut pending, 14),
        Some((12, b"after".as_slice()))
    );
    assert!(pending.is_empty());
}
#[test]
fn event_queue_preserves_opaque_payloads_nominal_arrival_and_same_time_sequence() {
    use std::collections::BTreeMap;
    // Queue contract only: this does not call World::route or establish that
    // later cut/heal effects preserve its routing decision or FIFO tail.
    let first = vec![0xff, 0, 0x80, b'\n', 1];
    let second = vec![0, 0xfe, b'\r', 0x81, 2];
    let later = vec![0x82, 0xff, 0, 3];
    let mut pending = BTreeMap::from([
        ((11, 2), second.clone()),
        ((11, 1), first.clone()),
        ((15, 3), later.clone()),
    ]);
    let frozen = pending.clone();
    assert!(events::pop_due_at(&mut pending, 9).is_none());
    assert_eq!(pending, frozen);
    // Equal-time insertion sequence and original nominal arrival are retained.
    assert_eq!(events::pop_due_at(&mut pending, 11), Some((11, first)));
    assert_eq!(events::pop_due_at(&mut pending, 11), Some((11, second)));
    assert!(events::pop_due_at(&mut pending, 11).is_none());
    assert_eq!(pending, BTreeMap::from([((15, 3), later.clone())]));
    assert!(events::pop_due_at(&mut pending, 14).is_none());
    // Late polling still reports nominal arrival, not polling time.
    assert_eq!(events::pop_due_at(&mut pending, 19), Some((15, later)));
    assert!(pending.is_empty());
}

#[test]
fn frames_meet_a_paused_or_down_receiver_as_it_is_when_they_arrive() {
    use events::{arrival, early_bound, Arrival};
    // Running receiver: queued whatever comes later.
    assert_eq!(arrival(5, 10, false, false, 0, 2), Arrival::Queue);
    // Paused until 10: held up to the backlog, then dropped.
    assert_eq!(arrival(5, 10, false, true, 0, 2), Arrival::Held);
    assert_eq!(arrival(9, 10, false, true, 1, 2), Arrival::Held);
    assert_eq!(arrival(9, 10, false, true, 2, 2), Arrival::Drop);
    // Arriving at or after the resume: an ordinary frame, not backlog.
    assert_eq!(arrival(10, 10, false, true, 2, 2), Arrival::Queue);
    // Down until the reboot at 10: no NIC to arrive at; afterwards, the new one.
    assert_eq!(arrival(9, 10, true, false, 0, 2), Arrival::Drop);
    assert_eq!(arrival(10, 10, true, false, 0, 2), Arrival::Queue);
    // Down with no reboot planned: nothing arrives.
    assert_eq!(
        arrival(u64::MAX - 1, u64::MAX, true, false, 0, 2),
        Arrival::Drop
    );

    // The early window stops short of a pause or crash at 100.
    assert_eq!(early_bound(50, 30, u64::MAX), 80);
    assert_eq!(early_bound(80, 30, 100), 99);
    assert_eq!(early_bound(99, 30, 100), 99);
    assert_eq!(early_bound(u64::MAX - 5, 30, u64::MAX), u64::MAX - 1);
}

/// At a reboot, everything that reached the dead machine before it (held
/// while paused, or arriving while down) is removed in arrival order; what
/// arrives from the reboot on stays for the new NIC.
#[test]
fn a_reboot_takes_only_what_arrived_before_it() {
    use std::collections::BTreeMap;
    let mut queue = BTreeMap::from([
        ((15, 4), "held"),
        ((5, 9), "paused backlog"),
        ((20, 2), "in transit at the reboot"),
        ((30, 1), "later"),
    ]);
    let mut gone = Vec::new();
    while let Some(e) = events::pop_before(&mut queue, 20) {
        gone.push(e);
    }
    assert_eq!(gone, vec![(5, "paused backlog"), (15, "held")]);
    assert_eq!(
        queue.into_values().collect::<Vec<_>>(),
        vec!["in transit at the reboot", "later"]
    );
}
