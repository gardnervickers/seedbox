//! Required causal handoff is one ordered drain.
use super::*;
use crate::driver::fixture::owned::*;

fn policy(driver: &mut Driver<Mock>) -> Vec<u8> {
    let mut bytes = Vec::new();
    driver
        .with_barrier(|barrier| {
            barrier.write_policy_evidence(&mut bytes).unwrap();
            Ok(())
        })
        .unwrap();
    bytes
}

#[test]
fn same_turn_admit_reject_and_continuation_share_one_drain() {
    let mut driver = build_network(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
        false,
    );
    driver
        .schedule(
            at(10),
            key("admit"),
            Action::Pause(Target::Machine(MachineId(1))),
        )
        .unwrap();
    driver
        .schedule(
            at(10),
            key("reject"),
            Action::Heal(HealTarget::Input(key("absent"))),
        )
        .unwrap();
    // Retain the required handoff until one drain.
    for _ in 0..1000 {
        if driver.now() >= at(40) {
            break;
        }
        driver.step(at(40)).unwrap();
    }
    assert_eq!(driver.now(), at(40));
    let batch = driver.drain_observations();
    let ordered: Vec<_> = batch
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::FaultAdmitted(record) => Some(("admitted", record.requested_at)),
            EventKind::InputRejected { .. } => Some(("rejected", event.at)),
            _ => None,
        })
        .collect();
    assert_eq!(ordered, [("admitted", at(10)), ("rejected", at(10))]);
    let checkpoint = driver.checkpoint().unwrap();
    let mut restored = restore(&checkpoint);
    advance(&mut driver, 80);
    advance(&mut restored, 80);
    assert_eq!(policy(&mut restored), policy(&mut driver));
    assert_eq!(states(&restored), states(&driver));
}
