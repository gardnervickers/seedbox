//! Incoming main checkpoint controls, using the canonical world coordinator.
use super::*;

fn world(undo: u64) -> World {
    let kvm = kvm_ioctls::Kvm::new().unwrap();
    // Increment a RAM marker on every counted loop iteration, then halt.
    let code = [
        0xb9, 0x10, 0x27, 0, 0, 0xff, 0x05, 0, 0, 8, 0, 0x49, 0x75, 0xf7, 0xf4,
    ];
    let mut kernel = vec![0u8; 1024];
    kernel[0x1f1] = 1;
    kernel.extend(code);
    let boot = BootInputs {
        kernel: kernel.into(),
        initrd: Rc::from([]),
        cmdline: (String::new(), String::new()),
        mem: 0x0810_0000,
        mac: None,
        control_channel: true,
        disk: Some(disk::Disk::new(None, Some(8)).unwrap()),
        undo_margin: undo,
        tolerance: 0,
        timer_jitter: 0,
        exact_timer_ppm: 0,
        margin: None,
        console_limit: None,
        pv_clock: false,
        page_timer: false,
        log_chunk_limit: None,
        deferred_tx: false,
    };
    let names = vec!["a".to_string(), "b".to_string()];
    let machines = names
        .iter()
        .map(|name| boot_machine(&kvm, name, &boot, false, [0; 32]).unwrap())
        .collect();
    World {
        failure: None,
        machines,
        factory: kvm,
        boot: vec![boot.clone(), boot],
        barrier: false,
        boot_seed: 7,
        run_seed: 7,
        branch_identity: None,
        incarnations: vec![vec![], vec![]],
        paused_ingress: vec![0; 2],
        clock: Clock::Counter(Counter::new().unwrap()),
        events: vec![BTreeMap::new(); 2],
        strict_events: vec![BTreeMap::new(); 2],
        seq: 0,
        lookahead: 100,
        early: 0,
        latency: 100,
        partitioned: BTreeMap::new(),
        faults: boot_faults(
            &names,
            &[],
            100,
            links::Links::from_config(7, &names, &json!({"latency_ns":100,"jitter_ns":10}))
                .unwrap(),
        )
        .unwrap(),
        control: control::Control::new(&Value::Null, names, 100, 0).unwrap(),
        macs: vec![[0, 0, 0, 0, 0, 1], [0, 0, 0, 0, 0, 2]],
        dropped: 0,
        delivered: 0,
        link_counts: BTreeMap::new(),
        semantic_counts: network_counters::NetworkCounters::default(),
        slices: 0,
    }
}

fn finish(world: &mut World) -> Vec<(String, String)> {
    simulate(world, &json!({}), 20_000, &None, None, &mut None).unwrap();
    world
        .machines
        .iter_mut()
        .map(|m| (m.exit_witness(), m.state_digest().0))
        .collect()
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn several_checkpoints_restore_nonmonotonically_and_repeat_branches() {
    for undo in [0, 256] {
        let mut w = world(undo);
        assert_eq!(
            simulate_to(
                &mut w,
                &json!({}),
                20_000,
                &None,
                None,
                Some(500),
                &mut None
            )
            .unwrap(),
            "checkpoint"
        );
        let first = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        let first_state: Vec<_> = w.machines.iter_mut().map(|m| m.state_digest().0).collect();
        assert_eq!(
            simulate_to(
                &mut w,
                &json!({}),
                20_000,
                &None,
                None,
                Some(800),
                &mut None
            )
            .unwrap(),
            "checkpoint"
        );
        let second = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        let want = finish(&mut w);
        second.restore_checkpoint(&mut w).unwrap();
        assert_eq!(
            finish(&mut w),
            want,
            "continuation from second checkpoint, undo={undo}"
        );
        first.restore_checkpoint(&mut w).unwrap();
        assert_eq!(
            w.machines
                .iter_mut()
                .map(|m| m.state_digest().0)
                .collect::<Vec<_>>(),
            first_state
        );
        let names = vec!["a".to_string(), "b".to_string()];
        reseed_checkpoint(
            &mut w,
            &names,
            7,
            42,
            &checkpoint::branch_identity("parent", 500, 42),
        )
        .unwrap();
        let branch = finish(&mut w);
        second.restore_checkpoint(&mut w).unwrap();
        finish(&mut w);
        first.restore_checkpoint(&mut w).unwrap();
        reseed_checkpoint(
            &mut w,
            &names,
            7,
            42,
            &checkpoint::branch_identity("parent", 500, 42),
        )
        .unwrap();
        assert_eq!(
            finish(&mut w),
            branch,
            "old checkpoint restored after newer one, undo={undo}"
        );
    }
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_keeps_lifecycle_disk_network_control_and_rng_state() {
    for undo in [0, 256] {
        let mut w = world(undo);
        let names = vec!["a".to_string(), "b".to_string()];
        w.control = control::Control::new(
            &json!({"machine":"a","latency_ns":1000,"placement":{"a":"a","b":"b"}}),
            names.clone(),
            100,
            0,
        )
        .unwrap();
        w.control
            .request(
                0,
                0,
                b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"inspect\"}",
                0,
            )
            .unwrap();
        assert_eq!(
            simulate_to(
                &mut w,
                &json!({}),
                20_000,
                &None,
                None,
                Some(500),
                &mut None
            )
            .unwrap(),
            "checkpoint"
        );
        w.schedule(
            1,
            900,
            Payload::Frame {
                from: 0,
                bytes: vec![1, 2, 3],
            },
        );
        w.strict_events[0].insert((950, 99), b"management\n".to_vec());
        w.partitioned.insert((0, 1), true);
        w.faults.links.last.insert((0, 1), 900);
        w.link_counts.entry((0, 1)).or_default()[0] += 1;
        w.semantic_counts.record(900, 0, 1, 0);
        w.lifecycle(1, faults::Lifecycle::Crash, 500).unwrap();
        w.lifecycle(1, faults::Lifecycle::Reboot, 500).unwrap();
        w.lifecycle(1, faults::Lifecycle::Pause, 500).unwrap();
        w.paused_ingress[1] = 2;
        w.machines[0].timer_rng = Some(Rng::new(7, "timer"));
        w.machines[0].timer_rng.as_mut().unwrap().next();
        let disk = w.machines[0].blk.as_mut().unwrap().disk_mut();
        disk.write(0, &[1; 512]).unwrap();
        disk.flush();
        disk.write(1, &[2; 512]).unwrap();
        let disk_digest = disk.digest();
        w.faults.management_ready(0, 300, 100).unwrap();
        w.faults
            .append(seedbox_control::Plan {
                id: 0,
                at_ns: 500,
                deadline_ns: 600,
                action: seedbox_control::Action::Partition {
                    handle: "held".into(),
                    left: vec!["a".into()],
                    right: vec!["b".into()],
                },
            })
            .unwrap();
        w.faults.apply_due(500);
        let handles = w.faults.active_handles();
        let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        let link_draw = w.faults.links.stream_sample((0, 1));
        let timer_draw = w.machines[0].timer_rng.as_mut().unwrap().next();
        w.events[1].clear();
        w.strict_events[0].clear();
        w.faults.error = Some("changed".into());
        w.lifecycle(0, faults::Lifecycle::Crash, 500).unwrap();
        saved.restore_checkpoint(&mut w).unwrap();
        assert_eq!(w.machines[1].incarnation, 1);
        assert!(w.machines[1].paused);
        assert_eq!(w.incarnations[1].len(), 1);
        assert_eq!(w.paused_ingress[1], 2);
        assert!(
            matches!(&w.events[1].values().next().unwrap(),Payload::Frame{from:0,bytes} if bytes==&vec![1,2,3])
        );
        assert_eq!(w.strict_events[0][&(950, 99)], b"management\n");
        assert_eq!(w.faults.links.last[&(0, 1)], 900);
        assert!(w.partitioned[&(0, 1)]);
        assert_eq!(
            w.machines[0].blk.as_ref().unwrap().disk().digest(),
            disk_digest
        );
        assert_eq!(w.faults.links.stream_sample((0, 1)), link_draw);
        assert_eq!(w.machines[0].timer_rng.as_mut().unwrap().next(), timer_draw);
        assert!(w.faults.error.is_none());
        assert_eq!(w.faults.readiness.len(), 1);
        assert_eq!(w.faults.active_handles(), handles);
        assert!(w.faults.blocked(0, 1));
        assert_eq!(w.faults.log.len(), 1);
        assert_eq!(w.control.boundary(), 1000);
        assert_eq!(w.control.pending.as_ref().unwrap().relay, 1);
        let pending = w.control.pending.as_ref().unwrap().request.clone();
        reseed_checkpoint(
            &mut w,
            &names,
            7,
            99,
            &checkpoint::branch_identity("parent", 500, 99),
        )
        .unwrap();
        assert_eq!(w.control.pending.as_ref().unwrap().request, pending);
        assert_eq!(w.run_seed, 99);
        assert_ne!(w.faults.links.stream_sample((0, 1)), link_draw);
    }
}
#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_branches_disk_survival_and_restores_down_survivors() {
    let mut w = world(0);
    let names = vec!["a".to_string(), "b".to_string()];
    let mut disk =
        disk::Disk::new(None, Some(64))
            .unwrap()
            .with_fault_model(disk::DiskFaultModel {
                sector_bytes: 512,
                survival_probability: 0.5,
            });
    disk.write(0, &[b'A'; 64 * 512]).unwrap();
    disk.flush();
    disk.write(0, &[b'B'; 64 * 512]).unwrap();
    w.machines[0].blk = Some(virtio_blk::VirtioBlk::new(disk.clone()));
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    let mut results = Vec::new();
    for (at, seed) in [(100, 7), (100, 42), (200, 42), (100, 42)] {
        saved.restore_checkpoint(&mut w).unwrap();
        let identity = checkpoint::branch_identity("parent", at, seed);
        reseed_checkpoint(&mut w, &names, 7, seed, &identity).unwrap();
        let expected = disk.crashed_branch(seed, w.branch_identity.as_deref(), "a", 0);
        w.lifecycle(0, faults::Lifecycle::Crash, 0).unwrap();
        assert_eq!(
            w.machines[0].blk.as_ref().unwrap().disk().digest(),
            expected.0.digest()
        );
        assert_eq!(w.incarnations[0][0]["disk_lost_sectors"], expected.1);
        results.push((expected.0.digest(), w.incarnations[0].clone()));
        let down = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        w.lifecycle(0, faults::Lifecycle::Reboot, 0).unwrap();
        down.restore_checkpoint(&mut w).unwrap();
        assert!(w.machines[0].down);
        assert_eq!(w.machines[0].incarnation, 1);
        reseed_checkpoint(
            &mut w,
            &names,
            seed,
            99,
            &checkpoint::branch_identity("child", at, 99),
        )
        .unwrap();
        assert_eq!(
            w.machines[0].blk.as_ref().unwrap().disk().digest(),
            expected.0.digest()
        );
    }
    assert_eq!(results[1], results[3]);
    assert_ne!(results[0].0, results[1].0);
    assert_ne!(results[1].0, results[2].0);
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_restores_pending_compose_lifecycle_calls() {
    for op in ["pause", "resume", "crash", "reboot"] {
        let mut w = world(0);
        let names = vec!["a".to_string(), "b".to_string()];
        w.control = control::Control::new(
            &json!({"machine":"a","latency_ns":1000,"placement":{"a":"a","b":"b"}}),
            names,
            100,
            0,
        )
        .unwrap();
        let prerequisite = match op {
            "resume" => Some(seedbox_control::Action::Pause {
                machine: "b".into(),
            }),
            "reboot" => Some(seedbox_control::Action::Crash {
                machine: "b".into(),
            }),
            _ => None,
        };
        if let Some(action) = prerequisite {
            w.faults
                .append(seedbox_control::Plan {
                    id: 0,
                    at_ns: 0,
                    deadline_ns: 100,
                    action,
                })
                .unwrap();
        }
        simulate_to(
            &mut w,
            &json!({}),
            20_000,
            &None,
            None,
            Some(500),
            &mut None,
        )
        .unwrap();
        let request = format!(
            "SBX_RPC1 1 1 {}",
            json!({"version":1,"id":1,"op":op,"target":{"service":"b"}})
        );
        w.control.request(0, 500, request.as_bytes(), 500).unwrap();
        simulate_to(
            &mut w,
            &json!({}),
            20_000,
            &None,
            None,
            Some(1000),
            &mut None,
        )
        .unwrap();
        let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        let mut results = Vec::new();
        for _ in 0..2 {
            saved.restore_checkpoint(&mut w).unwrap();
            assert_eq!(w.control.boundary(), 1500);
            simulate_to(
                &mut w,
                &json!({}),
                20_000,
                &None,
                None,
                Some(1600),
                &mut None,
            )
            .unwrap();
            assert!(w.control.pending.as_ref().unwrap().error.is_none());
            let m = &w.machines[1];
            assert_eq!(m.paused, op == "pause");
            assert_eq!(m.down, op == "crash");
            results.push((
                w.faults.log.clone(),
                w.incarnations.clone(),
                m.exit_witness(),
                w.control.boundary(),
            ));
        }
        assert_eq!(results[0], results[1], "{op}");
    }
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn boot_snapshots_require_every_machine_parked() {
    let mut w = world(0);
    assert!(WorldSnapshot::take(&mut w).is_err());
    w.machines[0].park().unwrap();
    assert!(WorldSnapshot::take(&mut w).is_err());
    w.machines[1].park().unwrap();
    assert!(WorldSnapshot::take(&mut w).is_ok());
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn parent_branch_continues_without_restarting_any_stream() {
    for undo in [0, 256] {
        let mut w = world(undo);
        let names = vec!["a".to_string(), "b".to_string()];
        simulate_to(
            &mut w,
            &json!({}),
            20_000,
            &None,
            None,
            Some(500),
            &mut None,
        )
        .unwrap();
        w.faults.links.stream_sample((0, 1));
        w.machines[0].timer_rng = Some(Rng::new(7, "timer"));
        w.machines[0].timer_rng.as_mut().unwrap().next();
        let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        let want_link = w.faults.links.stream_sample((0, 1));
        let want_timer = w.machines[0].timer_rng.as_mut().unwrap().next();
        let want = finish(&mut w);
        saved.restore_checkpoint(&mut w).unwrap();
        let before: Vec<_> = w.machines.iter_mut().map(|m| m.state_digest().0).collect();
        let reseed = reseed_checkpoint(
            &mut w,
            &names,
            7,
            7,
            &checkpoint::branch_identity("parent", 500, 7),
        )
        .unwrap();
        assert!(reseed.iter().all(|r| r["reseed"] == "continuation"));
        assert!(w.branch_identity.is_none());
        assert_eq!(
            w.machines
                .iter_mut()
                .map(|m| m.state_digest().0)
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(w.faults.links.stream_sample((0, 1)), want_link);
        assert_eq!(w.machines[0].timer_rng.as_mut().unwrap().next(), want_timer);
        assert_eq!(finish(&mut w), want, "undo={undo}");
    }
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_drains_fault_effects_and_control_requests_before_capture() {
    let mut w = world(0);
    let names = vec!["a".to_string(), "b".to_string()];
    w.control = control::Control::new(
        &json!({"machine":"a","latency_ns":1000,"placement":{"a":"a","b":"b"}}),
        names,
        100,
        0,
    )
    .unwrap();
    simulate_to(
        &mut w,
        &json!({}),
        20_000,
        &None,
        None,
        Some(500),
        &mut None,
    )
    .unwrap();
    // Model messages produced by the last slice before the scheduler
    // considers T. A direct snapshot is unsafe; simulate_to drains them.
    w.machines[0].fault_effects.push((0, 499, true));
    w.machines[0].control_requests.push((
        499,
        b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"inspect\"}".to_vec(),
    ));
    assert!(WorldSnapshot::take_checkpoint(&mut w).is_err());
    assert_eq!(
        simulate_to(
            &mut w,
            &json!({}),
            20_000,
            &None,
            None,
            Some(500),
            &mut None
        )
        .unwrap(),
        "checkpoint"
    );
    assert!(w.machines[0].fault_effects.is_empty());
    assert!(w.machines[0].control_requests.is_empty());
    assert_eq!(w.faults.readiness, vec![json!({"machine":"a","at_ns":499})]);
    assert_eq!(w.control.boundary(), 1499);
    assert!(w.control.pending.is_some());
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    saved.restore_checkpoint(&mut w).unwrap();
    assert_eq!(w.control.boundary(), 1499);
    assert_eq!(w.faults.readiness.len(), 1);
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_reseeds_paused_guests_and_reports_down_guests() {
    let mut w = world(0);
    let names = vec!["a".to_string(), "b".to_string()];
    w.machines[0].pause().unwrap();
    let before = w.machines[0].published_entropy();
    w.lifecycle(1, faults::Lifecycle::Crash, 0).unwrap();
    let reseed = reseed_checkpoint(
        &mut w,
        &names,
        7,
        42,
        &checkpoint::branch_identity("parent", 0, 42),
    )
    .unwrap();
    assert_eq!(reseed[0]["reseed"], "time_page");
    assert_eq!(reseed[1]["reseed"], "not_up");
    assert!(w.machines[0].paused);
    assert_eq!(w.machines[0].time, 0);
    assert_ne!(w.machines[0].published_entropy(), before);
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_identity_keys_live_streams_and_future_incarnations() {
    let mut w = world(0);
    let names = vec!["a".to_string(), "b".to_string()];
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    let mut draws = Vec::new();
    for at in [100, 200, 100] {
        saved.restore_checkpoint(&mut w).unwrap();
        let identity = checkpoint::branch_identity("parent", at, 42);
        reseed_checkpoint(&mut w, &names, 7, 42, &identity).unwrap();
        let link = w.faults.links.stream_sample((0, 1));
        let timer = w.machines[0].timer_rng.as_mut().unwrap().next();
        let page = w.machines[0].published_entropy();
        let mut expected = w.faults.links.clone();
        expected.reseed_branch(42, Some(&identity), &names);
        assert_eq!(link, expected.stream_sample((0, 1)));
        assert_eq!(
            timer,
            Rng::branch(42, Some(&identity), "timer-jitter/a/0").next()
        );
        w.lifecycle(0, faults::Lifecycle::Crash, 0).unwrap();
        w.lifecycle(0, faults::Lifecycle::Reboot, 0).unwrap();
        let reboot_timer = w.machines[0].timer_rng.as_mut().unwrap().next();
        assert_eq!(
            reboot_timer,
            Rng::branch(42, Some(&identity), "timer-jitter/a/1").next()
        );
        draws.push((
            link,
            timer,
            page,
            reboot_timer,
            w.machines[0].state_digest().0,
        ));
    }
    assert_eq!(draws[0], draws[2]);
    assert_ne!(draws[0].0, draws[1].0);
    assert_ne!(draws[0].1, draws[1].1);
    assert_ne!(draws[0].2, draws[1].2);
    assert_ne!(draws[0].3, draws[1].3);
    assert_ne!(draws[0].4, draws[1].4);
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_keeps_partial_matchers_selectors_and_pending_trigger_effects() {
    let mut w = world(256);
    let names = vec!["a".to_string(), "b".to_string()];
    let action = seedbox_control::Action::Partition {
        handle: "trigger-cut".into(),
        left: vec!["a".into()],
        right: vec!["b".into()],
    };
    w.control
        .triggers
        .arm("a".into(), b"abab".to_vec(), 0, 1000, action.clone())
        .unwrap();
    w.control
        .triggers
        .arm("a".into(), b"x".to_vec(), 0, 400, action)
        .unwrap();
    w.control
        .triggers
        .committed("a", 300, b'a', 100, 300)
        .unwrap();
    w.control
        .triggers
        .committed("a", 301, b'b', 100, 301)
        .unwrap();
    simulate_to(
        &mut w,
        &json!({}),
        20_000,
        &None,
        None,
        Some(500),
        &mut None,
    )
    .unwrap();
    // The last byte from the last completed slice has not reached the matcher yet.
    w.machines[0].committed_console = Some((499, b'a'));
    simulate_to(
        &mut w,
        &json!({}),
        20_000,
        &None,
        None,
        Some(500),
        &mut None,
    )
    .unwrap();
    assert_eq!(w.control.triggers.records[0].prefix, 3);
    assert_eq!(w.control.triggers.records[1].status, "expired");
    let mut syn = vec![0u8; 54];
    syn[12..14].copy_from_slice(&[8, 0]);
    syn[14] = 0x45;
    syn[16..18].copy_from_slice(&40u16.to_be_bytes());
    syn[23] = 6;
    syn[26..30].copy_from_slice(&[10, 0, 0, 1]);
    syn[30..34].copy_from_slice(&[10, 0, 0, 2]);
    syn[34..36].copy_from_slice(&1234u16.to_be_bytes());
    syn[36..38].copy_from_slice(&80u16.to_be_bytes());
    syn[46] = 0x50;
    syn[47] = 2;
    let session = w.faults.connections.observe(0, 1, &syn).unwrap();
    let connection = w
        .faults
        .connections
        .list("a", "b", 0, &names)
        .pop()
        .unwrap();
    w.faults
        .connections
        .select(99, "selected".into(), connection, &names)
        .unwrap();
    assert!(w.faults.connections.drops(Some(session), false, |_| {}));
    w.faults
        .links
        .shape(
            98,
            "shaped".into(),
            std::collections::BTreeSet::from([(0, 1)]),
            links::Policy {
                latency_ns: 200,
                jitter_ns: 10,
                loss_ppm: 500_000,
                bandwidth_bps: None,
                queue_bound_ns: None,
            },
        )
        .unwrap();
    w.faults.links.decide(0, 1, 499, 1500, false);
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    let mut evidence = Vec::new();
    for _ in 0..2 {
        saved.restore_checkpoint(&mut w).unwrap();
        assert_eq!(w.faults.connections.selections["selected"].matched, 1);
        assert_eq!(w.faults.connections.selections["selected"].dropped, 1);
        assert_eq!(w.faults.links.layers.len(), 1);
        let mut hash = Sha256::new();
        w.faults.connections.witness(&mut hash);
        w.faults.links.witness(&mut hash);
        w.control
            .triggers
            .committed("a", 500, b'b', 100, 500)
            .unwrap();
        assert_eq!(w.control.triggers.boundary(), 600);
        let matched = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        simulate_to(
            &mut w,
            &json!({}),
            20_000,
            &None,
            None,
            Some(700),
            &mut None,
        )
        .unwrap();
        let want = w.control.triggers.evidence().unwrap();
        let faults = w.faults.log.clone();
        matched.restore_checkpoint(&mut w).unwrap();
        assert_eq!(w.control.triggers.boundary(), 600);
        simulate_to(
            &mut w,
            &json!({}),
            20_000,
            &None,
            None,
            Some(700),
            &mut None,
        )
        .unwrap();
        assert_eq!(w.control.triggers.evidence().unwrap(), want);
        assert_eq!(w.faults.log, faults);
        assert!(w.faults.blocked(0, 1));
        assert_eq!(w.control.triggers.records[0].status, "fired");
        evidence.push((hash.finalize().to_vec(), want, faults));
        w.faults.connections.selections.clear();
        w.faults.links.layers.clear();
    }
    assert_eq!(evidence[0], evidence[1]);
}

#[test]
#[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn checkpoint_drain_preserves_trigger_target_exit_notification() {
    let mut w = world(0);
    w.faults.management_ready(1, 0, 100).unwrap();
    w.control
        .triggers
        .arm(
            "a".into(),
            b"x".to_vec(),
            0,
            1000,
            seedbox_control::Action::Stop {
                machine: "b".into(),
            },
        )
        .unwrap();
    w.control.triggers.committed("a", 0, b'x', 100, 0).unwrap();
    w.control
        .triggers
        .apply_boundary(&mut w.faults, 100)
        .unwrap();
    assert_eq!(w.faults.apply_due(100).len(), 1);
    w.machines[1].time = 100;
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    for _ in 0..2 {
        saved.restore_checkpoint(&mut w).unwrap();
        drain_machine_messages(&mut w, 1, false, true).unwrap();
        assert_eq!(w.faults.log[0]["status"], "unavailable");
        assert_eq!(
            w.faults.log[0]["detail"],
            "target VM exited before the process effect"
        );
    }
}

/// Build artifacts/entropy-fork.cpio as described in harness/worlds/README.md.
#[test]
#[ignore = "requires ABI 3 artifacts/bzImage, artifacts/entropy-fork.cpio, exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
fn runtime_checkpoint_getrandom_and_urandom_repeat_and_diverge() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut w = world(256);
    let mut boot = w.boot[0].clone();
    boot.kernel = std::fs::read(root.join("artifacts/bzImage"))
        .unwrap()
        .into();
    boot.initrd = std::fs::read(root.join("artifacts/entropy-fork.cpio"))
        .unwrap()
        .into();
    boot.mem = 256 * 1024 * 1024;
    boot.disk = None;
    boot.control_channel = false;
    boot.cmdline=("console=ttyS0 nokaslr noapic nolapic acpi=off io_delay=none tsc=reliable tsc_early_khz=1000000 lpj=1000000 clocksource=tsc random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4 rdinit=/init".into(),String::new());
    w.boot = vec![boot.clone(), boot.clone()];
    let names = vec!["a".to_string(), "b".to_string()];
    w.machines = names
        .iter()
        .map(|name| {
            boot_machine(
                &w.factory,
                name,
                &boot,
                false,
                entropy_seed(7, &format!("boot-entropy/{name}")),
            )
            .unwrap()
        })
        .collect();
    let at = 2_000_000_000;
    let duration = 10_000_000_000;
    w.lookahead = 1_000_000;
    w.latency = 1_000_000;
    assert_eq!(
        simulate_to(
            &mut w,
            &json!({}),
            duration,
            &None,
            None,
            Some(at),
            &mut None
        )
        .unwrap(),
        "checkpoint"
    );
    for m in &w.machines {
        let prefix = String::from_utf8_lossy(&m.com1.out);
        assert!(prefix.contains("before fork:"));
        assert!(!prefix.contains("after fork:"));
    }
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    let mut outputs = Vec::new();
    for seed in [42, 43, 42] {
        saved.restore_checkpoint(&mut w).unwrap();
        let report = reseed_checkpoint(
            &mut w,
            &names,
            7,
            seed,
            &checkpoint::branch_identity("parent", at, seed),
        )
        .unwrap();
        assert!(report.iter().all(|r| r["reseed"] == "time_page"));
        simulate(&mut w, &json!({}), duration, &None, None, &mut None).unwrap();
        outputs.push(
            w.machines
                .iter()
                .map(|m| {
                    assert_eq!(m.guest_exit_code, Some(0));
                    let output = String::from_utf8_lossy(&m.com1.out);
                    let line = output
                        .lines()
                        .find(|l| l.starts_with("after fork:"))
                        .unwrap();
                    let values = line.split_whitespace().skip(2).collect::<Vec<_>>();
                    assert_eq!(values.len(), 2);
                    for (value, source) in values.iter().zip(["getrandom=", "urandom="]) {
                        let hex = value.strip_prefix(source).unwrap();
                        assert_eq!(hex.len(), 32, "empty or short {source} output");
                        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
                    }
                    values.into_iter().map(str::to_owned).collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(outputs[0], outputs[2]);
    for machine in 0..2 {
        assert_eq!(outputs[0][machine].len(), 2);
        for source in 0..2 {
            assert_ne!(outputs[0][machine][source], outputs[1][machine][source]);
        }
    }
}
