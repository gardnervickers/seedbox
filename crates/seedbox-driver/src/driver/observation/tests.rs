//! Required causal handoff is independent of optional retention.
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
fn zero_event_capacity_keeps_same_turn_admit_reject_and_continuation() {
    let mut reference = None;
    for workers in [1, 4] {
        for capacity in [0, usize::MAX] {
            let mut driver = build(
                vec![spec(0, true, vec![]), spec(1, true, vec![])],
                vec![],
                true,
            );
            driver.set_event_capacity(capacity);
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
            advance(&mut driver, 40);
            let batch = driver.drain_observations();
            let ordered: Vec<_> = batch
                .changes
                .iter()
                .filter_map(|change| match &change.change {
                    CommittedChange::FaultAdmitted(record) => {
                        Some(("admitted", record.requested_at))
                    }
                    CommittedChange::InputRejected { .. } => Some(("rejected", change.at)),
                    _ => None,
                })
                .collect();
            assert_eq!(ordered, [("admitted", at(10)), ("rejected", at(10))]);
            if capacity == 0 {
                assert!(batch.events.is_empty());
                assert!(batch.omitted_events > 0);
            }
            let before = policy(&mut driver);
            let checkpoint = driver.checkpoint().unwrap();
            let mut restored = restore(&checkpoint, if workers == 1 { 4 } else { 1 });
            advance(&mut driver, 80);
            advance(&mut restored, 80);
            assert_eq!(policy(&mut restored), policy(&mut driver));
            assert_eq!(states(&restored), states(&driver));
            let actual = (batch.changes, before, states(&driver), policy(&mut driver));
            if let Some(expected) = &reference {
                assert_eq!(&actual, expected);
            } else {
                reference = Some(actual);
            }
        }
    }
}

#[test]
fn event_overflow_is_watermarked_and_resumed_sequence_has_a_gap() {
    let mut driver = build(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
    );
    let initial = driver.drain_observations();
    driver.set_event_capacity(1);
    driver
        .with_barrier(|barrier| {
            barrier.add(|| Ok(Mock::new(spec(2, true, vec![]))))?;
            barrier.add(|| Ok(Mock::new(spec(3, true, vec![]))))?;
            Ok(())
        })
        .unwrap();
    let overflow = driver.drain_observations();
    assert_eq!(overflow.events.len(), 1);
    assert_eq!(overflow.events[0].sequence, initial.next_event_sequence);
    assert_eq!(overflow.omitted_events, 1);
    assert_eq!(
        overflow.next_event_sequence,
        initial.next_event_sequence + 2
    );
    driver
        .with_barrier(|barrier| barrier.add(|| Ok(Mock::new(spec(4, true, vec![])))))
        .unwrap();
    let resumed = driver.drain_observations();
    assert_eq!(resumed.omitted_events, 0);
    assert_eq!(resumed.events.len(), 1);
    assert_eq!(resumed.events[0].sequence, overflow.next_event_sequence);
    assert_eq!(resumed.events[0].sequence, overflow.events[0].sequence + 2);
}
