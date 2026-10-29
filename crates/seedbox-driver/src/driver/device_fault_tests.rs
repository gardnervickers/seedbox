//! Disk and scheduler faults: Driver admission, handles, machine requests,
//! heal counts and continuation, with owned mock machines.
use super::fixture::owned::*;
use super::*;
use crate::pv::{DeviceFault, DiskEffect, DiskOp, DiskScope, FaultCounts};

fn target(index: usize) -> Target {
    Target::Name(format!("fixture-{index}"))
}
fn disk_error() -> DeviceFault {
    DeviceFault::DiskError {
        op: DiskOp::Write,
        ppm: 1_000,
        effect: DiskEffect::Eio,
        scope: DiskScope::All,
    }
}
fn handle(name: &str) -> FaultHandle {
    FaultHandle::named(name.to_owned()).unwrap()
}
fn fault(d: &mut Driver<Mock>, when: u64, name: &str, machine: usize, fault: DeviceFault) {
    d.with_barrier(|b| {
        b.schedule_with_handle(
            at(when),
            key(name),
            Action::DeviceFault {
                target: target(machine),
                fault,
            },
            handle(name),
        )
    })
    .unwrap();
}
fn heal(d: &mut Driver<Mock>, when: u64, name: &str) {
    d.schedule(
        at(when),
        key(&format!("heal-{name}")),
        Action::Heal(HealTarget::Handle(handle(name))),
    )
    .unwrap();
}
fn machines() -> Vec<State> {
    vec![
        spec(0, true, vec![(0, Response::Ready)]),
        spec(1, true, vec![(0, Response::Ready)]),
    ]
}
/// Settled faults among events: (fault, action, status, counts).
fn settled(events: &[Event]) -> Vec<(u32, FaultAction, SettlementStatus, Option<FaultCounts>)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultSettled {
                record,
                status,
                counts,
                ..
            } => Some((record.fault.get(), record.action, *status, *counts)),
            _ => None,
        })
        .collect()
}
fn rejected(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::InputRejected { key, .. } => Some(key.as_str().to_owned()),
            _ => None,
        })
        .collect()
}
/// Device fault requests a machine received, in order.
fn requests(d: &Driver<Mock>, machine: usize) -> Vec<Request> {
    states(d)[machine]
        .received
        .iter()
        .map(|input| input.payload.clone())
        .filter(|payload| {
            matches!(
                payload,
                Request::InstallFault { .. } | Request::HealFault { .. }
            )
        })
        .collect()
}

#[test]
fn a_device_fault_installs_in_its_machine_and_its_heal_settles_with_counts() {
    let mut d = build(machines(), vec![], true);
    fault(&mut d, 5, "eio", 1, disk_error());
    heal(&mut d, 15, "eio");
    let events = advance(&mut d, 30);
    let settled = settled(&events);
    let [(installed, FaultAction::DeviceFault(action), SettlementStatus::Applied, None), (_, FaultAction::Heal, SettlementStatus::Applied, Some(counts))] =
        settled.as_slice()
    else {
        panic!("{settled:?}")
    };
    assert_eq!(*action, disk_error());
    let requests = requests(&d, 1);
    let [Request::InstallFault { id, seed, fault }, Request::HealFault { id: healed }] =
        requests.as_slice()
    else {
        panic!("{requests:?}")
    };
    assert_eq!((*fault, healed), (disk_error(), id));
    assert_eq!(
        *seed,
        d.execution.control.fault_seed(MachineId(1), *installed)
    );
    assert_eq!(
        *counts,
        FaultCounts {
            matched: *id,
            affected: id / 2
        }
    );
    assert!(requests_of(&d, 0).is_empty(), "only the target receives it");
    // The heal released the handle and its input key.
    assert!(d.execution.inputs.is_empty());
}
fn requests_of(d: &Driver<Mock>, machine: usize) -> Vec<Request> {
    requests(d, machine)
}

#[test]
fn device_faults_apply_in_every_state_and_outlive_crash_and_reboot() {
    let mut d = build(machines(), vec![], true);
    d.schedule(at(2), key("crash"), Action::Crash(target(1)))
        .unwrap();
    fault(
        &mut d,
        4,
        "slow",
        1,
        DeviceFault::DiskLatency {
            op: DiskOp::Any,
            latency_ns: 1_000,
            jitter_ns: 10,
        },
    );
    d.schedule(at(6), key("reboot"), Action::Reboot(target(1)))
        .unwrap();
    heal(&mut d, 12, "slow");
    let events = advance(&mut d, 30);
    let settled = settled(&events);
    assert!(
        settled
            .iter()
            .all(|(_, _, status, _)| *status == SettlementStatus::Applied),
        "{settled:?}"
    );
    assert!(matches!(
        settled.last(),
        Some((_, FaultAction::Heal, _, Some(_)))
    ));
    assert_eq!(requests(&d, 1).len(), 2);
}

#[test]
fn one_preempt_fault_per_machine_while_disk_faults_stack() {
    let mut d = build(machines(), vec![], true);
    fault(&mut d, 5, "race", 1, DeviceFault::Preempt { ppm: 10 });
    fault(&mut d, 6, "again", 1, DeviceFault::Preempt { ppm: 20 });
    fault(&mut d, 6, "other", 0, DeviceFault::Preempt { ppm: 20 });
    fault(&mut d, 7, "first", 1, disk_error());
    fault(&mut d, 7, "second", 1, disk_error());
    let events = advance(&mut d, 20);
    assert_eq!(rejected(&events), ["again"]);
    assert_eq!(requests(&d, 1).len(), 3);
    assert_eq!(requests(&d, 0).len(), 1);
    // After a heal the machine may be preempted again.
    heal(&mut d, 25, "race");
    fault(&mut d, 26, "later", 1, DeviceFault::Preempt { ppm: 30 });
    let events = advance(&mut d, 40);
    assert!(rejected(&events).is_empty());
}

#[test]
fn an_active_device_fault_continues_from_a_checkpoint() {
    let mut d = build(machines(), vec![], true);
    fault(&mut d, 5, "eio", 1, disk_error());
    heal(&mut d, 25, "eio");
    advance(&mut d, 12);
    let checkpoint = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 40);
    let expected_states = states(&d);
    assert!(matches!(
        settled(&expected).as_slice(),
        [(_, FaultAction::Heal, SettlementStatus::Applied, Some(_))]
    ));
    for workers in [1, 3] {
        let mut branch = restore(&checkpoint, workers);
        assert_eq!(advance(&mut branch, 40), expected);
        assert_eq!(states(&branch), expected_states);
    }
}
