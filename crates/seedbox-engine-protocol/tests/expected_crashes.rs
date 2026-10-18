//! Expected crashes end to end at the protocol boundary: a world document
//! declares them, the supervisor's management lines carry kernel evidence,
//! and the shared reducer turns both into the reported assessment.
use seedbox_engine_protocol::property::{
    decode, AssessmentStatus, Ledger, PropertyStatus, DEFAULT_NO_CRASH,
};
use seedbox_engine_protocol::world_config::WorldConfig;
use serde_json::json;

/// The management line the supervisor writes for one of its own records.
fn supervisor_line(seq: u64, record: serde_json::Value) -> Vec<u8> {
    let hex: String = serde_json::to_vec(&record)
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("SBX_PROP1 0 {seq} {hex}").into_bytes()
}

fn reduce(world: &WorldConfig, lines: &[(&str, u64, &str)]) -> Ledger {
    let expected = world.expected_crashes();
    let mut ledger = Ledger::default();
    for (machine, seq, message) in lines {
        let index = world
            .machines
            .iter()
            .position(|m| m.name == *machine)
            .unwrap();
        let line = supervisor_line(*seq, json!({"kernel": {"message": message}}));
        let frame = decode(&line).unwrap().unwrap();
        ledger.apply_expecting(&frame, machine, 0, *seq * 10, &expected[index]);
    }
    ledger
}

#[test]
fn a_declared_watchdog_abort_passes_and_anything_else_still_violates() {
    let world: WorldConfig = serde_json::from_value(json!({
        "kernel": "kernel",
        "machines": [
            {"name": "s3", "initrd": "s3",
             "expected_crashes": [{"signal": 6, "command": "keel-watchdog"}]},
            {"name": "oracle", "initrd": "oracle"}
        ]
    }))
    .unwrap();
    let abort = "libdynamorio.so: keel-watchdog: potentially unexpected fatal signal 6.";

    let ledger = reduce(&world, &[("s3", 1, abort), ("s3", 2, abort)]);
    let assessment = ledger.assessment();
    assert_eq!(assessment.status, AssessmentStatus::Passed);
    let result = &assessment.properties[0];
    assert_eq!(result.id, DEFAULT_NO_CRASH);
    assert_eq!(result.status, PropertyStatus::Passed);
    assert_eq!((result.passes, result.failures), (2, 0));
    let details = result.example.as_ref().unwrap().details.as_ref().unwrap();
    assert_eq!(details["expected"]["command"], "keel-watchdog");

    // The same abort on a machine that declares nothing is a violation, and
    // the expected one stays counted beside it.
    let ledger = reduce(&world, &[("s3", 1, abort), ("oracle", 1, abort)]);
    let assessment = ledger.assessment();
    assert_eq!(assessment.status, AssessmentStatus::Violated);
    let result = &assessment.properties[0];
    assert_eq!((result.passes, result.failures), (1, 1));
    assert_eq!(result.example.as_ref().unwrap().origin.machine, "oracle");

    // The ledger, which reports and witnesses carry, round-trips.
    let restored: Ledger = serde_json::from_value(serde_json::to_value(&ledger).unwrap()).unwrap();
    assert_eq!(restored, ledger);
}
