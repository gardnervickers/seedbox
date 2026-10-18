use super::*;
fn target() -> HandleTarget {
    HandleTarget::Links {
        left: vec!["a".into()],
        right: vec!["b".into()],
    }
}
fn named(h: &mut Handles, text: &str) -> ExecutionPlan {
    h.named(target(), Change::Partition, text.into()).unwrap()
}
fn allocated(h: &mut Handles) -> ExecutionPlan {
    h.network(target(), Change::Partition).unwrap()
}
#[test]
fn duplicate_pending_names_have_distinct_identity_and_only_the_loser_is_removed() {
    let mut h = Handles::default();
    let a = named(&mut h, "same");
    let b = named(&mut h, "same");
    assert_ne!(a.record_id, b.record_id);
    assert_eq!(h.records.len(), 2);
    assert!(!h.collision(&a));
    h.applied(&a).unwrap();
    assert!(h.collision(&b));
    h.rejected(&b).unwrap();
    assert_eq!(h.active_handles().unwrap(), ["same"]);
    assert!(h.records.contains_key(&a.record_id));
    let heal = h.heal("same").unwrap();
    assert_eq!(heal.record_id, a.record_id);
    h.applied(&heal).unwrap();
    assert!(h.active_handles().unwrap().is_empty());
    assert_eq!(allocated(&mut h).handle.to_string(), "p1");
}
#[test]
fn mixed_text_kinds_collide_in_both_apply_orders_and_consumed_pn_stays_consumed() {
    for named_first in [true, false] {
        let mut h = Handles::default();
        let (first, second) = if named_first {
            (named(&mut h, "p1"), allocated(&mut h))
        } else {
            (allocated(&mut h), named(&mut h, "p1"))
        };
        h.applied(&first).unwrap();
        assert!(h.collision(&second), "named_first={named_first}");
        h.rejected(&second).unwrap();
        assert_eq!(h.active_handles().unwrap(), ["p1"]);
        assert_eq!(h.records.len(), 1);
        assert_eq!(allocated(&mut h).handle.to_string(), "p2");
    }
}
#[test]
fn text_heal_prefers_active_then_healing_and_lost_over_pending() {
    for lost in [false, true] {
        let mut h = Handles::default();
        let a = h.step("a".into(), 1).unwrap();
        let b = h
            .named(
                HandleTarget::Machine("b".into()),
                Change::Step(2),
                "p1".into(),
            )
            .unwrap();
        h.applied(&a).unwrap();
        if lost {
            h.machine_ended("a");
        }
        let heal = h.heal("p1").unwrap();
        assert_eq!(heal.record_id, a.record_id);
        assert_eq!(heal.lost, lost);
        assert_eq!(h.heal("p1"), Err(Error::Unsettled));
        assert!(h.collision(&b));
        h.rejected(&b).unwrap();
        h.applied(&heal).unwrap();
        assert!(h.active_handles().unwrap().is_empty());
    }
}
#[test]
fn heal_by_input_identity_never_resolves_an_active_same_text_neighbor() {
    let mut h = Handles::default();
    let a = named(&mut h, "x");
    let b = named(&mut h, "x");
    h.applied(&a).unwrap();
    assert!(h.active_record(a.record_id));
    assert!(!h.active_record(b.record_id));
    assert_eq!(h.heal_record(b.record_id), Err(Error::Unsettled));
    assert!(h.active_record(a.record_id));
    let heal = h.heal_record(a.record_id).unwrap();
    assert_eq!(heal.record_id, a.record_id);
    h.rejected(&b).unwrap();
    h.applied(&heal).unwrap();
    assert!(h.active_handles().unwrap().is_empty());
}
#[test]
fn admission_checks_nonpending_text_only_and_empty_is_the_only_name_limit() {
    let mut h = Handles::default();
    assert_eq!(
        h.named(target(), Change::Partition, String::new()),
        Err(Error::InactiveHandle)
    );
    let text = " \0é".repeat(33_000);
    let p = named(&mut h, &text);
    let pending = named(&mut h, &text);
    h.applied(&p).unwrap();
    assert_eq!(
        h.named(target(), Change::Partition, text.clone()),
        Err(Error::DuplicateActive)
    );
    h.rejected(&pending).unwrap();
    assert_eq!(h.active_handles().unwrap(), [text]);
    assert_eq!(h.allocator.last, 0);
}
#[test]
fn persisted_identity_highwater_allocator_and_text_uniqueness_are_independent() {
    let mut h = Handles::default();
    let a = named(&mut h, "p1");
    let b = allocated(&mut h);
    h.check_persist(&|_| true).unwrap();
    h.applied(&a).unwrap();
    h.records.get_mut(&b.record_id).unwrap().phase = Phase::Active;
    assert!(h.check_persist(&|_| true).is_err());
    h.records.get_mut(&b.record_id).unwrap().phase = Phase::Pending;
    let saved = h.clone();
    h.last_record = 1;
    assert!(h.check_persist(&|_| true).is_err());
    h = saved.clone();
    h.allocator.last = 0;
    assert!(h.check_persist(&|_| true).is_err());
    h = saved;
    h.records.get_mut(&a.record_id).unwrap().text = Text::Named(String::new());
    assert!(h.check_persist(&|_| true).is_err());
}
