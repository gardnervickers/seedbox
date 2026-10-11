use super::*;
use crate::world_mock::*;

#[test]
fn admission_rejects_invalid_worlds_before_execution() {
    fn construct(w: WorldInputs<Mock>) -> Result<World<Mock>, String> {
        World::new(w)
    }
    assert!(construct(world_inputs(&[]))
        .err()
        .unwrap()
        .contains("at least one"));
    let mut inputs = world_inputs(&["a"]);
    inputs.boot.clear();
    assert!(construct(inputs).err().unwrap().contains("lengths"));
    let mut inputs = world_inputs(&["a"]);
    inputs.early = inputs.lookahead;
    assert!(construct(inputs).err().unwrap().contains("slack"));
    let fresh = construct(world_inputs(&["a", "b"])).unwrap();
    assert_eq!(fresh.events().len(), 2);
    assert!(fresh.events().iter().all(BTreeMap::is_empty));
    assert_eq!(
        (fresh.delivered(), fresh.dropped(), fresh.slices()),
        (0, 0, 0)
    );
    let mut w = world(&["a", "b"]);
    w.boot.pop();
    assert!(w.validate().unwrap_err().contains("lengths"));
    assert!(w.factory.lock().unwrap().is_empty());
    let w = world(&[""]);
    assert!(w.validate().is_ok());
    let w = world(&["a", "a"]);
    assert!(w.validate().is_ok());
    let mut w = world(&["a", "b"]);
    w.macs[1] = w.macs[0];
    assert!(w.validate().is_ok());
    let names = (0..257).map(|i| format!("machine-{i}")).collect::<Vec<_>>();
    let names = names.iter().map(String::as_str).collect::<Vec<_>>();
    assert!(world(&names).validate().is_ok());
    let mut w = world(&["a"]);
    w.early = w.lookahead;
    assert!(w.validate().unwrap_err().contains("slack"));
}

#[test]
fn invalid_scheduler_checkpoint_is_rejected_before_physical_restore() {
    for fresh in [false, true] {
        let mut w = world(&["a", "b"]);
        let scheduler = crate::window::World::<crate::world_windowed::Msg>::new(
            2,
            crate::window::Params {
                latency: 10,
                early: 0,
                duration: 100,
                end_on: None,
                max_outputs: 64,
            },
        )
        .unwrap();
        w.scheduler_continuation = Some(scheduler);
        let mut saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        let encoded = serde_json::to_vec(&saved.scheduler_continuation).unwrap();
        saved.scheduler_continuation = serde_json::from_slice(&encoded).unwrap();
        let mut corrupt = serde_json::to_value(&saved.scheduler_continuation).unwrap();
        corrupt["version"] = json!(999);
        saved.scheduler_continuation = Some(serde_json::from_value(corrupt).unwrap());
        w.machines[0].time = 10;
        let before = w.factory.lock().unwrap().clone();
        let error = if fresh {
            saved.restore_checkpoint(&mut w)
        } else {
            saved.restore(&mut w)
        }
        .unwrap_err();
        assert!(error.contains("unsupported version"));
        assert_eq!(w.machines[0].time, 10);
        assert_eq!(*w.factory.lock().unwrap(), before);
        assert!(w.failure.is_none());
        // Replacing the invalid metadata permits a later valid restore.
        saved.scheduler_continuation = serde_json::from_slice(&encoded).unwrap();
        saved.restore_checkpoint(&mut w).unwrap();
        assert_eq!(w.machines[0].time, 0);
        assert!(w.scheduler_continuation.is_some());
    }
}

#[test]
fn shared_lifecycle_guards_preserve_failure_and_commit_points() {
    let mut s = MachineState::new("a".into());
    assert!(s.begin_release().is_err());
    assert!(!s.parked);
    s.pause().unwrap();
    assert!(s.pause().is_err());
    assert!(s.paused);
    s.time = 9;
    s.resume(7).unwrap();
    assert_eq!(s.time, 9);
    assert!(s.check_park(true).is_err());
    assert!(!s.parked);
    s.check_park(false).unwrap();
    assert!(!s.parked);
    s.commit_park();
    assert!(s.parked);
    assert!(s.pause().is_err());
    s.begin_release().unwrap();
    assert!(!s.parked);
    assert!(s.boot_at(20).is_err());
    s.down = true;
    assert!(s.boot_at(8).is_err());
    assert!(s.down);
    s.boot_at(20).unwrap();
    assert_eq!(s.booted_at, 20);
    let mut old = s.clone();
    old.incarnation = u32::MAX;
    assert!(s.succeed(&old, 30).is_err());
    assert!(!s.down);
    assert_eq!(s.time, 20);
}
