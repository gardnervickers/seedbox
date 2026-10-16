use super::*;
use seedbox_artifacts::execution::StopReason;
use seedbox_engine_protocol::choice::{ChoiceEvidence, ChoiceRecord};
use seedbox_engine_protocol::property::Ledger;
use serde_json::json;

fn origin(machine: &str, incarnation: u32, time_ns: u64) -> serde_json::Value {
    json!({"machine": machine, "incarnation": incarnation, "session": 1, "seq": 1, "timeNs": time_ns})
}

#[test]
fn links_a_triggered_crash_to_its_lifecycle_and_the_violation_and_lists_omissions() {
    let id: SimulationId = "k7m2p9qx".parse().unwrap();
    let program: Vec<ProgramInput> = serde_json::from_value(json!([
        {"key": "a0", "atNs": 100, "action": {"action": "anchor"}},
        {"key": "mutate-3", "atNs": 100, "action": {"action": "trigger",
            "when": {"on": "event", "name": "commit"}, "then": {"action": "crash", "machine": "db"}}},
        {"key": "gen-0", "atNs": 900, "action": {"action": "crash", "machine": "db"}},
    ]))
    .unwrap();
    let world: WorldState = serde_json::from_value(json!({
        "simulation": "k7m2p9qx", "revision": 3, "simulatedTimeNs": 2000,
        "machines": [], "links": [],
        "faults": [
            {"id": 1, "inputKey": "mutate-3", "action": "crash", "machine": "db",
             "status": "applied", "appliedTimeNs": 400, "active": false},
            {"id": 2, "inputKey": "gen-0", "action": "crash", "machine": "db",
             "status": "failed", "active": false}
        ],
        "triggers": {"mutate-3": {"status": "fired", "seen": 2, "matchedAtNs": 350,
            "effectAtNs": 400, "faultId": 1}}
    }))
    .unwrap();
    let events: Vec<SimulationEvent> = serde_json::from_value(json!([
        {"simulation": "k7m2p9qx", "revision": 3, "simulatedTimeNs": 400,
         "event": {"type": "machineLifecycle", "machine": "db", "incarnation": 0,
                   "transition": {"type": "crashed"}}},
        {"simulation": "k7m2p9qx", "revision": 3, "event": {"type": "stopRequested"}}
    ]))
    .unwrap();
    let mut ledger = Ledger::default();
    ledger.properties.insert(
        "acknowledged writes survive".into(),
        serde_json::from_value(json!({
            "kind": "always", "declared": true, "passes": 4, "failures": 1,
            "firstPass": {"origin": origin("oracle", 0, 200)},
            "firstFailure": {"origin": origin("oracle", 0, 1500), "details": {"lost": 3}}
        }))
        .unwrap(),
    );
    ledger.events.insert("commit".into(), 5);
    ledger.event_first.insert(
        "commit".into(),
        serde_json::from_value(origin("db", 0, 300)).unwrap(),
    );
    ledger.states.insert(
        "db/phase=recovering".into(),
        serde_json::from_value(origin("db", 1, 600)).unwrap(),
    );
    let report = ExecutionReport {
        host_wait_ns: 0,
        simulated_time_ns: Some(2000),
        reason: StopReason::Completed,
        workload_exit: None,
        witness: None,
        full_state: None,
        faults: Vec::new(),
        detail: None,
        properties: Some(ledger),
        choices: Some(ChoiceEvidence {
            records: vec![ChoiceRecord {
                machine: "oracle".into(),
                incarnation: 0,
                session: 2,
                site: "client-0/retry".into(),
                occurrence: 1,
                alternatives: 2,
                selected: 1,
                chosen: "yes".into(),
                requested_at_ns: 1200,
                reply_at_ns: 1201,
                input_key: None,
            }],
            untracked: 0,
        }),
    };
    let explained = project(&id, &program, &world, &events, 1, Some(&report));
    let times: Vec<u64> = explained.entries.iter().map(|e| e.at_ns).collect();
    assert!(
        times.windows(2).all(|w| w[0] <= w[1]),
        "time ordered: {times:?}"
    );
    let find = |id: &str, kind: Kind| {
        explained
            .entries
            .iter()
            .find(|e| e.id == id && e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} {id}"))
    };
    assert_eq!(
        find("trigger:mutate-3", Kind::Trigger).links,
        ["input:mutate-3", "fault:1"]
    );
    assert_eq!(find("fault:1", Kind::Fault).links, ["trigger:mutate-3"]);
    assert_eq!(find("machine:db#0", Kind::Lifecycle).links, ["fault:1"]);
    // A failed fault keeps its scheduled time and its status.
    let refused = find("fault:2", Kind::Fault);
    assert_eq!(
        (refused.at_ns, refused.detail.as_str()),
        (900, "crash db: failed")
    );
    let failed = explained
        .entries
        .iter()
        .find(|e| e.id == "property:acknowledged writes survive" && e.violation)
        .unwrap();
    assert_eq!(failed.at_ns, 1500);
    assert!(failed.detail.contains("\"lost\":3"));
    assert_eq!(find("event:commit", Kind::Event).detail, "first of 5");
    assert_eq!(
        find("state:db/phase=recovering", Kind::State).incarnation,
        Some(1)
    );
    assert!(find("choice:oracle/client-0/retry#1", Kind::Choice)
        .detail
        .contains("chose yes (2 of 2) drawn"));
    let omissions = explained.omissions.join("\n");
    assert!(omissions.contains("commit ×5"), "{omissions}");
    assert!(omissions.contains("acknowledged writes survive (4 passed, 1 failed)"));
    assert!(omissions.contains("1 lifecycle and fault events"));
}

#[test]
fn a_missing_report_is_an_omission() {
    let id: SimulationId = "k7m2p9qx".parse().unwrap();
    let world: WorldState = serde_json::from_value(json!({
        "simulation": "k7m2p9qx", "revision": 1, "simulatedTimeNs": 0,
        "machines": [], "links": [], "faults": []
    }))
    .unwrap();
    let explained = project(&id, &[], &world, &[], 0, None);
    assert!(explained.entries.is_empty());
    assert_eq!(explained.omissions.len(), 1);
}

#[test]
fn a_reached_unreachable_property_is_the_violation() {
    let id: SimulationId = "k7m2p9qx".parse().unwrap();
    let world: WorldState = serde_json::from_value(json!({
        "simulation": "k7m2p9qx", "revision": 1, "simulatedTimeNs": 0,
        "machines": [], "links": [], "faults": []
    }))
    .unwrap();
    let mut ledger = Ledger::default();
    ledger.properties.insert(
        "bad path".into(),
        serde_json::from_value(json!({
            "kind": "unreachable", "declared": true, "passes": 1, "failures": 0,
            "firstPass": {"origin": origin("db", 0, 70)}
        }))
        .unwrap(),
    );
    let report = ExecutionReport {
        host_wait_ns: 0,
        simulated_time_ns: Some(100),
        reason: StopReason::Completed,
        workload_exit: None,
        witness: None,
        full_state: None,
        faults: Vec::new(),
        detail: None,
        properties: Some(ledger),
        choices: None,
    };
    let explained = project(&id, &[], &world, &[], 0, Some(&report));
    let entry = &explained.entries[0];
    assert!(entry.violation);
    assert_eq!(entry.detail, "first reached");
}
