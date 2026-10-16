use super::*;
fn config(n: u32) -> ExecutionConfig {
    ExecutionConfig::new(n).unwrap()
}
#[test]
fn shared_numbering_and_later_state_rejection_consume_handles() {
    let mut handles = Handles::default();
    assert_eq!(handles.reserve().unwrap().to_string(), "p1");
    let first = handles.throttle("a".into(), config(2000)).unwrap();
    assert_eq!(first.handle.to_string(), "p2");
    assert!(handles.active_handles().unwrap().is_empty());
    assert_eq!(
        handles.throttle("a".into(), config(3000)),
        Err(Error::AlreadyThrottled)
    );
    handles.applied(&first).unwrap();
    assert_eq!(
        handles.throttle("a".into(), config(3000)),
        Err(Error::AlreadyThrottled)
    );
    let independent = handles.throttle("b".into(), config(1001)).unwrap();
    assert_eq!(independent.handle.to_string(), "p5");
    handles.applied(&independent).unwrap();
    assert_eq!(handles.active_handles().unwrap(), ["p2", "p5"]);
}
#[test]
fn checked_exhaustion_never_wraps_or_mutates_prior_state() {
    let mut handles = Handles {
        allocator: Allocator { last: u32::MAX - 1 },
        ..Default::default()
    };
    let last = handles.throttle("a".into(), config(1001)).unwrap();
    assert_eq!(last.handle.to_string(), "p4294967295");
    handles.applied(&last).unwrap();
    assert_eq!(handles.reserve(), Err(Error::HandlesExhausted));
    assert_eq!(handles.allocator.last, u32::MAX);
    assert_eq!(handles.active_handles().unwrap(), ["p4294967295"]);
    let heal = handles.heal("p4294967295").unwrap();
    assert_eq!(heal.change, Change::Execution(ExecutionConfig::default()));
    handles.applied(&heal).unwrap();
    assert!(handles.active_handles().unwrap().is_empty());
}
#[test]
fn heal_removes_only_matching_handle_and_restores_nominal() {
    let mut handles = Handles::default();
    let a = handles.throttle("a".into(), config(4000)).unwrap();
    let b = handles.throttle("b".into(), config(2000)).unwrap();
    assert_eq!(handles.heal("p1"), Err(Error::Unsettled));
    handles.applied(&a).unwrap();
    handles.applied(&b).unwrap();
    for inactive in ["", "p0", "p01", "p3", "unknown"] {
        assert_eq!(handles.heal(inactive), Err(Error::InactiveHandle));
    }
    let heal = handles.heal("p1").unwrap();
    assert_eq!(
        (heal.target.clone(), heal.change),
        (
            HandleTarget::Machine("a".into()),
            Change::Execution(ExecutionConfig::default())
        )
    );
    assert_eq!(handles.heal("p1"), Err(Error::Unsettled));
    handles.applied(&heal).unwrap();
    assert_eq!(handles.active_handles().unwrap(), ["p2"]);
    assert_eq!(handles.heal("p1"), Err(Error::InactiveHandle));
    assert_eq!(handles.reserve().unwrap().to_string(), "p3");
}
#[test]
fn rejected_admission_is_distinct_from_uncertain_execution_failure() {
    let mut handles = Handles::default();
    let rejected = handles.throttle("a".into(), config(2000)).unwrap();
    handles.rejected(&rejected).unwrap();
    assert!(handles.active_handles().unwrap().is_empty());
    let active = handles.throttle("a".into(), config(3000)).unwrap();
    assert_eq!(active.handle.to_string(), "p2");
    handles.applied(&active).unwrap();
    let rejected_heal = handles.heal("p2").unwrap();
    handles.rejected(&rejected_heal).unwrap();
    assert_eq!(handles.active_handles().unwrap(), ["p2"]);
    let uncertain_heal = handles.heal("p2").unwrap();
    handles.execution_failed();
    assert_eq!(handles.applied(&uncertain_heal), Err(Error::Poisoned));
    assert_eq!(handles.rejected(&uncertain_heal), Err(Error::Poisoned));
    assert_eq!(handles.active_handles(), Err(Error::Poisoned));
    assert_eq!(handles.reserve(), Err(Error::Poisoned));
}
#[test]
fn stale_confirmation_cannot_complete_a_later_heal_attempt() {
    let mut handles = Handles::default();
    let active = handles.throttle("a".into(), config(3000)).unwrap();
    handles.applied(&active).unwrap();
    let old = handles.heal("p1").unwrap();
    handles.rejected(&old).unwrap();
    let current = handles.heal("p1").unwrap();
    assert_ne!(old.revision, current.revision);
    assert_eq!(handles.applied(&old), Err(Error::UnexpectedEffect));
    assert_eq!(handles.applied(&current), Err(Error::Poisoned));
}
#[test]
fn service_state_copy_keeps_active_records_independent() {
    // Machine-only resets have no ledger callback: the service owns these
    // records independently of the driver-owned execution configuration. This
    // clone test is not whole-simulation snapshot/native acceptance.
    let mut handles = Handles::default();
    let plan = handles.throttle("a".into(), config(2000)).unwrap();
    handles.applied(&plan).unwrap();
    let mut whole_service_copy = handles.clone();
    let heal = handles.heal("p1").unwrap();
    handles.applied(&heal).unwrap();
    assert!(handles.active_handles().unwrap().is_empty());
    assert_eq!(whole_service_copy.active_handles().unwrap(), ["p1"]);
    assert_eq!(
        whole_service_copy.heal("p1").unwrap().change,
        Change::Execution(ExecutionConfig::default())
    );
}
#[test]
fn inspection_order_and_heal_attempt_exhaustion_preserve_active_state() {
    let mut handles = Handles::default();
    for index in 1..=10 {
        let plan = handles.throttle(format!("m{index}"), config(2000)).unwrap();
        handles.applied(&plan).unwrap();
    }
    assert_eq!(
        handles.active_handles().unwrap(),
        ["p1", "p10", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"]
    );
    let record = handles.records.get_mut(&RecordId(1)).unwrap();
    record.revision = u64::MAX;
    assert_eq!(handles.heal("p1"), Err(Error::AttemptsExhausted));
    let record = &handles.records[&RecordId(1)];
    assert_eq!(record.phase, Phase::Active);
    assert_eq!(record.revision, u64::MAX);
    assert_eq!(handles.active_handles().unwrap().len(), 10);
}
#[test]
fn drifts_do_not_stack_and_heal_to_nominal_keeping_kind() {
    let rate = |ppb| ClockRate::new(ppb).unwrap();
    let mut handles = Handles::default();
    let throttle = handles.throttle("a".into(), config(2000)).unwrap();
    handles.applied(&throttle).unwrap();
    // Throttle and drift are independent categories on one machine.
    let drift = handles.drift("a".into(), rate(5)).unwrap();
    assert!(handles.drifting("a") && !handles.drifting("b"));
    assert_eq!(
        handles.drift("a".into(), rate(6)),
        Err(Error::AlreadyDrifting)
    );
    handles.applied(&drift).unwrap();
    assert_eq!(
        handles.drift("a".into(), rate(6)),
        Err(Error::AlreadyDrifting)
    );
    // The machine ending leaves a drift active: it is a host dependency.
    handles.machine_ended("a");
    let heal = handles.heal(&drift.handle.to_string()).unwrap();
    assert_eq!(heal.change, Change::Drift(ClockRate::NOMINAL));
    assert!(!heal.lost);
    handles.applied(&heal).unwrap();
    assert!(!handles.drifting("a"));
    assert_eq!(handles.active_handles().unwrap(), ["p1"]);
}
#[test]
fn steps_stack_heal_inversely_and_become_lost_with_their_boot() {
    let mut handles = Handles::default();
    let first = handles.step("a".into(), -7).unwrap();
    let second = handles.step("a".into(), 9).unwrap();
    // A pending step is unaffected by the machine ending: its fault settles.
    handles.machine_ended("a");
    handles.applied(&first).unwrap();
    handles.applied(&second).unwrap();
    let heal = handles.heal("p1").unwrap();
    assert_eq!((heal.change, heal.lost), (Change::Step(7), false));
    handles.rejected(&heal).unwrap();
    handles.machine_ended("a");
    assert_eq!(handles.active_handles().unwrap(), ["p1", "p2"]);
    let heal = handles.heal("p1").unwrap();
    assert_eq!((heal.change, heal.lost), (Change::Step(0), true));
    // A refused lost heal remains lost and healable.
    handles.rejected(&heal).unwrap();
    let heal = handles.heal("p1").unwrap();
    assert!(heal.lost);
    handles.applied(&heal).unwrap();
    assert_eq!(handles.active_handles().unwrap(), ["p2"]);
    assert_eq!(handles.heal("p1"), Err(Error::InactiveHandle));
}
#[test]
fn steps_keep_the_bound_that_makes_their_inverse_representable() {
    let mut handles = Handles::default();
    let bound = super::super::MAX_CLOCK_OFFSET_NS as i64;
    for outside in [i64::MIN, -bound - 1, bound + 1, i64::MAX] {
        assert_eq!(
            handles.step("a".into(), outside),
            Err(Error::StepOutOfRange)
        );
    }
    assert!(handles.active_handles().unwrap().is_empty());
    for edge in [-bound, bound] {
        let plan = handles.step("a".into(), edge).unwrap();
        handles.applied(&plan).unwrap();
        let heal = handles.heal(&plan.handle.to_string()).unwrap();
        assert_eq!(heal.change, Change::Step(-edge));
        handles.applied(&heal).unwrap();
    }
    // A corrupt out-of-bound record cannot produce a wrapped inverse.
    let plan = handles.step("a".into(), 1).unwrap();
    handles.applied(&plan).unwrap();
    handles.records.get_mut(&plan.record_id).unwrap().change = Change::Step(i64::MIN);
    assert_eq!(
        handles.heal(&plan.handle.to_string()),
        Err(Error::StepOutOfRange)
    );
    assert_eq!(handles.records[&plan.record_id].phase, Phase::Active);
}
#[test]
fn partitions_stack_on_the_shared_allocator_and_heal_only_their_links() {
    let mut handles = Handles::default();
    let throttle = handles.throttle("a".into(), config(2000)).unwrap();
    let links = || (vec!["a".to_string()], vec!["b".to_string()]);
    let first = handles.partition(links().0, links().1).unwrap();
    let second = handles.partition(links().0, links().1).unwrap();
    assert_eq!(
        [
            throttle.handle.clone(),
            first.handle.clone(),
            second.handle.clone()
        ]
        .map(|h| h.to_string()),
        ["p1", "p2", "p3"]
    );
    handles.applied(&first).unwrap();
    handles.applied(&second).unwrap();
    // A partition is not machine state: the machine ending changes nothing.
    handles.machine_ended("a");
    let heal = handles.heal("p2").unwrap();
    assert_eq!(heal.change, Change::Unpartition);
    assert_eq!(
        heal.target,
        HandleTarget::Links {
            left: links().0,
            right: links().1
        }
    );
    handles.applied(&heal).unwrap();
    assert_eq!(handles.active_handles().unwrap(), ["p3"]);
}
