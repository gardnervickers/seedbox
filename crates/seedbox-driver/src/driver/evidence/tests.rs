//! Actual SDK construction, policy admissions and continuations; no native VM.
use super::*;
use crate::driver::fixture::owned::*;
fn evidence(d: &mut Driver<Mock>) -> Vec<u8> {
    let mut bytes = Vec::new();
    d.with_barrier(|b| {
        b.write_policy_evidence(&mut bytes).unwrap();
        Ok(())
    })
    .unwrap();
    bytes
}
#[test]
fn policy_projection_does_not_capture_native_state_draw_rng_or_retain_publication_drains() {
    let mut d = build(
        vec![
            spec(0, true, vec![(1, output(b"oracle"))]),
            spec(1, true, vec![(1, output(b"worker"))]),
        ],
        vec![],
        true,
    );
    advance(&mut d, 20);
    d.step(at(30)).unwrap();
    let captures: Vec<_> = d
        .with_barrier(|b| {
            Ok(b.machines()
                .map(|(_, machine)| machine.captures.clone())
                .collect())
        })
        .unwrap();
    let before = evidence(&mut d);
    assert!(captures
        .iter()
        .all(|count| count.load(std::sync::atomic::Ordering::SeqCst) == 0));
    d.with_barrier(|b| {
        b.set_rate(PlaybackRate::Limited {
            numerator: nz(3),
            denominator: nz(2),
        })
    })
    .unwrap();
    assert_eq!(evidence(&mut d), before);
    d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    assert!(!d.drain_events().is_empty());
    d.drain_observations();
    assert_eq!(evidence(&mut d), before);
    assert_eq!(evidence(&mut d), before);
    let captured = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 60);
    let expected_states = states(&d);
    let mut reference = restore(&captured, 1);
    assert_eq!(advance(&mut reference, 60), expected);
    assert_eq!(states(&reference), expected_states);
    assert_eq!(evidence(&mut reference), evidence(&mut d));
}
#[test]
fn actual_pending_and_applied_policy_not_only_events_enters_the_projection() {
    let mut d = build(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
    );
    let target = d
        .with_barrier(|b| Ok(b.machines().nth(1).unwrap().0))
        .unwrap();
    let before = evidence(&mut d);
    d.with_barrier(|b| b.schedule(at(20), key("pause"), Action::Pause(Target::Machine(target))))
        .unwrap();
    let pending = evidence(&mut d);
    assert_ne!(pending, before);
    advance(&mut d, 30);
    assert_ne!(evidence(&mut d), pending);
}
#[test]
fn logical_policy_continuation_is_independent_of_worker_resources() {
    let mut d = build(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
    );
    advance(&mut d, 20);
    let captured = d.checkpoint().unwrap();
    let expected = evidence(&mut d);
    for workers in [1, 4] {
        let mut branch = restore(&captured, workers);
        assert_eq!(evidence(&mut branch), expected);
        advance(&mut branch, 80);
        let mut reference = restore(&captured, 1);
        advance(&mut reference, 80);
        assert_eq!(evidence(&mut branch), evidence(&mut reference));
    }
}
#[test]
fn optional_frame_capture_capacity_and_draining_never_change_policy_evidence() {
    let make = |capacity| {
        let definitions: Vec<_> = (0..2)
            .map(|index| {
                let mut definition = crate::driver::fixture::definition(index);
                definition.network = Some(
                    crate::definition::NetworkInterface::new(
                        crate::definition::MacAddress::new(mac(index)).unwrap(),
                        std::net::Ipv4Addr::new(10, 0, 0, (index + 1) as u8),
                        24,
                    )
                    .unwrap(),
                );
                definition
            })
            .collect();
        let all: BTreeMap<_, _> = [
            spec(
                0,
                true,
                vec![
                    (1, frame(mac(1), 7)),
                    (2, frame(mac(1), 8)),
                    (3, frame(mac(1), 9)),
                ],
            ),
            spec(1, true, vec![]),
        ]
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
        let setup = NetworkSetup::new(
            NetworkConfig::new(Duration::from_nanos(5), Duration::from_nanos(3), 0).unwrap(),
            None,
            None,
            vec![],
        )
        .unwrap()
        .with_frame_capacity(capacity);
        Driver::new_seeded(
            at(0),
            Duration::from_nanos(10),
            1,
            51,
            || Ok(Context(None)),
            definitions[0].clone(),
            vec![definitions[1].clone()],
            vec![],
            setup,
            move |d| Ok(Mock::new(all[&d.name].clone())),
        )
        .unwrap()
    };
    let mut quiet = make(0);
    let mut observed = make(0);
    observed.set_frame_capacity(8);
    let mut disabled = make(8);
    advance(&mut disabled, 50);
    disabled.set_frame_capacity(0);
    let (encoded, store) = artifact(disabled.checkpoint().unwrap());
    let captured = decode(&encoded, &store);
    let mut restored_cleared = restore(&captured, 4);
    let cleared = disabled.drain_observations().frames;
    assert_eq!(
        restored_cleared.drain_observations().frames,
        FrameFacts::default()
    );
    assert!(cleared.facts.is_empty());
    assert_eq!(cleared.omitted, 3);
    assert_eq!(disabled.drain_observations().frames, FrameFacts::default());
    advance(&mut quiet, 50);
    advance(&mut observed, 50);
    let before = evidence(&mut observed);
    assert_eq!(evidence(&mut quiet), before);
    assert_eq!(evidence(&mut disabled), before);
    observed.set_frame_capacity(1);
    let facts = observed.drain_observations().frames;
    assert_eq!(facts.facts.len(), 1);
    assert_eq!(facts.omitted, 2);
    assert_eq!(evidence(&mut observed), before);

    // Capture while deliveries are still pending. Restore host observation
    // independently of the captured buffer, with either worker count.
    let mut source = make(8);
    advance(&mut source, 3);
    let checkpoint = source.checkpoint().unwrap();
    let retained = source.drain_observations().frames;
    assert!(!retained.facts.is_empty());
    let mut uninterrupted = make(0);
    advance(&mut uninterrupted, 80);
    let expected = evidence(&mut uninterrupted);
    for workers in [1, 4] {
        for capacity in [0, 8] {
            let mut branch = restore(&checkpoint, workers);
            branch.set_frame_capacity(capacity);
            let observations = branch.drain_observations().frames;
            assert_eq!(observations, FrameFacts::default());
            advance(&mut branch, 80);
            assert_eq!(evidence(&mut branch), expected);
        }
    }
}
#[test]
fn external_rng_cannot_be_invented_or_replaced_by_a_seeded_projection() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], false);
    let mut bytes = Vec::new();
    let result = d
        .with_barrier(|b| Ok(b.write_policy_evidence(&mut bytes)))
        .unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("owned seeded Driver RNG"));
    assert!(bytes.is_empty());
}
/// Network, control, scheduled-work and keyed-input policy: parts of the projection.
fn components(d: &Driver<Mock>) -> [Vec<u8>; 4] {
    let x = &d.execution;
    let mut network = Encoder { output: Vec::new() };
    x.network.policy_evidence(&mut network).unwrap();
    let mut control = Encoder { output: Vec::new() };
    x.control.put(&mut control).unwrap();
    let mut scheduled = Encoder { output: Vec::new() };
    x.events.put(&mut scheduled).unwrap();
    let mut inputs = Encoder { output: Vec::new() };
    x.inputs.put(&mut inputs).unwrap();
    [
        network.output,
        control.output,
        scheduled.output,
        inputs.output,
    ]
}
/// Each one-component mutation changes the projection, and only the part that
/// owns it: neither a queued frame nor a pending fault can be silently ignored
/// or misattributed by the comparison.
#[test]
fn one_component_mutations_change_only_their_own_part_of_the_projection() {
    let mut source = build_network(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
        true,
    );
    advance(&mut source, 10);
    assert!(source.execution.network.is_idle());
    let captured = source.checkpoint().unwrap();
    let mut base = restore(&captured, 1);
    let original = (evidence(&mut base), components(&base));
    let mut again = restore(&captured, 4);
    assert_eq!((evidence(&mut again), components(&again)), original);
    let changed = |d: &mut Driver<Mock>| {
        let parts = components(d);
        assert_ne!(evidence(d), original.0, "the projection ignored a change");
        [0, 1, 2, 3].map(|part| parts[part] != original.1[part])
    };

    // One link-queue entry: a frame in flight between the two machines.
    let mut queued = restore(&captured, 1);
    let Response::TransmitFrame { bytes } = frame(mac(1), 9) else {
        unreachable!()
    };
    let now = queued.now().as_nanos();
    queued
        .execution
        .network
        .route(MachineId(0), now, &bytes)
        .unwrap();
    assert!(!queued.execution.network.is_idle());
    assert_eq!(
        changed(&mut queued),
        [true, false, false, false],
        "queued frame"
    );

    // One pending host fault, with links and control untouched.
    let mut pending = restore(&captured, 1);
    pending
        .with_barrier(|b| {
            b.schedule(
                at(30),
                key("pause"),
                Action::Pause(Target::Machine(MachineId(1))),
            )
        })
        .unwrap();
    assert_eq!(
        changed(&mut pending),
        [false, false, true, true],
        "pending fault"
    );

    // An applied partition is network state held by its input's handle; once
    // applied it is no longer scheduled work, and healing changes the network
    // part again.
    let mut cut = restore(&captured, 1);
    cut.with_barrier(|b| {
        b.schedule(
            at(10),
            key("cut"),
            Action::Partition {
                left: vec![Target::Machine(MachineId(0))],
                right: vec![Target::Machine(MachineId(1))],
            },
        )
    })
    .unwrap();
    cut.step(at(10)).unwrap();
    let applied = components(&cut);
    assert_ne!(applied[0], original.1[0], "partition left the network part");
    assert_eq!(
        applied[2], original.1[2],
        "applied partition is still scheduled"
    );
    cut.with_barrier(|b| {
        b.schedule(
            at(10),
            key("heal"),
            Action::Heal(HealTarget::Input(key("cut"))),
        )
    })
    .unwrap();
    cut.step(at(10)).unwrap();
    let healed = components(&cut);
    assert_ne!(healed[0], applied[0], "heal left the network part");
    assert_eq!(healed[2], original.1[2]);
}
