//! The windowed scheduler over the scripted mock machine: the legacy loop's
//! scenarios, with results that must not depend on the worker count or on
//! which machine finishes a round first.
use super::*;
use crate::world::{release, reseed_checkpoint, WorldSnapshot};
use crate::world_mock::*;
use seedbox_control::{Action, Plan};
use sha2::Digest;

fn run(w: &mut World<Mock>, duration: u64, end_on: Option<&str>) -> Result<&'static str, String> {
    segment(w, &Value::Null, duration, end_on, None, None, true)
}

#[allow(clippy::too_many_arguments)]
fn segment(
    w: &mut World<Mock>,
    cfg: &Value,
    duration: u64,
    end_on: Option<&str>,
    fork: Option<Option<u64>>,
    checkpoint: Option<u64>,
    require_barrier: bool,
) -> Result<&'static str, String> {
    let end_on = end_on.map(str::to_owned);
    let run = Run {
        cfg,
        duration,
        end_on: &end_on,
        fork,
        checkpoint,
        require_barrier,
    };
    simulate(w, &run, Duration::ZERO, &mut || Ok(())).map(|(word, _)| word)
}

fn run_with(
    w: &mut World<Mock>,
    duration: u64,
    end_on: Option<&str>,
    workers: usize,
    skew: Duration,
) -> Result<(&'static str, Value), String> {
    let end_on = end_on.map(str::to_owned);
    let run = Run {
        cfg: &Value::Null,
        duration,
        end_on: &end_on,
        fork: None,
        checkpoint: None,
        require_barrier: true,
    };
    if workers == 0 {
        simulate(w, &run, skew, &mut || Ok(()))
    } else {
        simulate_parallel(
            w,
            &run,
            Workers {
                count: workers,
                skew,
            },
            |_| Ok(MockClock::default()),
            &mut || Ok(()),
        )
    }
}

fn trace(w: &World<Mock>) -> Vec<String> {
    w.factory.lock().unwrap().clone()
}

/// One machine's trace, in its own order: the only order a pool preserves.
fn machine_trace(w: &World<Mock>, name: &str) -> Vec<String> {
    trace(w)
        .into_iter()
        .filter(|line| line.starts_with(&format!("{name}:")))
        .collect()
}

fn configure_control(w: &mut World<Mock>) {
    let names = w.machines.iter().map(|m| m.name.clone()).collect();
    w.control = control::Control::new(
        &json!({"machine":"a","latency_ns":50,"placement":{}}),
        names,
        w.lookahead,
        w.early,
    )
    .unwrap();
}

#[test]
fn dynamic_network_faults_and_healing_are_identical_across_worker_counts_and_snapshots() {
    let mut reference = None;
    for workers in [0, 1, 2, 4] {
        let mut w = world(&["a", "b"]);
        w.early = 0;
        for plan in [
            json!({"id":1,"at_ns":4,"deadline_ns":4,"action":"shape","handle":"s","from":["a"],"to":["b"],"latency_ns":10,"jitter_ns":0,"loss_ppm":0,"network_faults":{"reorder_ppm":500_000,"reorder_delay_ns":30,"duplicate_ppm":500_000,"burst_loss_ppm":100_000,"burst_length":3,"latency_spike_ppm":500_000,"latency_spike_ns":20,"tcp_reset_ppm":500_000}}),
            json!({"id":2,"at_ns":30,"deadline_ns":30,"action":"heal","handle":"s"}),
        ] {
            w.faults
                .append(serde_json::from_value(plan).unwrap())
                .unwrap();
        }
        let mut frame = vec![0; 55];
        frame[5] = 1;
        frame[12..14].copy_from_slice(&[8, 0]);
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&41u16.to_be_bytes());
        frame[23] = 6;
        frame[34..38].copy_from_slice(&[0x0f, 0xa0, 0, 80]);
        frame[46] = 0x50;
        frame[47] = 0x18;
        for at in 5..40 {
            let mut step = Step::new(at, Stop::Sent);
            step.frames = vec![frame.clone()];
            w.machines[0].steps.push_back(step);
        }
        let snapshot = WorldSnapshot::take_checkpoint(&mut w).unwrap();
        w.factory.lock().unwrap().clear();
        let execute = |w: &mut World<Mock>| {
            run_with(w, 100, None, workers, Duration::from_micros(3)).unwrap();
            let mut h = sha2::Sha256::new();
            w.faults.network_witness(&mut h);
            let counts = &w.faults.log[1]["network_fault_counts"];
            for key in [
                "reordered",
                "duplicated",
                "burst_dropped",
                "spiked",
                "tcp_reset",
            ] {
                assert!(counts[key].as_u64().unwrap() > 0, "{counts}");
            }
            assert!(w.faults.active_handles().is_empty());
            (
                w.network_snapshot(),
                h.finalize().to_vec(),
                machine_trace(w, "b"),
                w.faults.log.clone(),
            )
        };
        let observed = execute(&mut w);
        if let Some(want) = &reference {
            assert_eq!(&observed, want);
        } else {
            reference = Some(observed.clone());
        }
        snapshot.restore(&mut w).unwrap();
        w.factory.lock().unwrap().clear();
        assert_eq!(execute(&mut w), observed);
    }
}

#[test]
fn bandwidth_tail_drop_and_nominal_counts_are_identical_across_worker_counts() {
    let mut reference = None;
    for workers in [0, 1, 2, 4] {
        let mut w = world(&["a", "b"]);
        w.early = 0;
        let names = vec!["a".into(), "b".into()];
        w.faults.links = crate::links::Links::from_config(
            1,
            &names,
            &json!({"latency_ns":10,"bandwidth_bps":1_000_000_000_000u64,"queue_bound_ns":12}),
        )
        .unwrap();
        let mut frame = vec![0; 1500];
        frame[5] = 1;
        let mut send = Step::new(5, Stop::Sent);
        send.frames = vec![frame.clone(), frame.clone()];
        let mut later = Step::new(17, Stop::Sent);
        later.frames = vec![frame];
        w.machines[0].steps.extend([send, later]);
        assert_eq!(
            run_with(&mut w, 45, None, workers, Duration::ZERO)
                .unwrap()
                .0,
            "duration"
        );
        assert_eq!(w.delivered, 2);
        assert_eq!(w.dropped, 1);
        assert_eq!(
            w.network_snapshot(),
            json!({"a->b":{"delivered":2,"partition_dropped":0,"receiver_dropped":0,"queue_dropped":1}})
        );
        assert!(machine_trace(&w, "b").contains(&"b:frame:27".into()));
        assert!(machine_trace(&w, "b").contains(&"b:frame:39".into()));
        let mut h = sha2::Sha256::new();
        w.faults.network_witness(&mut h);
        let observed = (
            w.network_snapshot(),
            h.finalize().to_vec(),
            machine_trace(&w, "b"),
        );
        if let Some(expected) = &reference {
            assert_eq!(&observed, expected);
        } else {
            reference = Some(observed);
        }
    }
}

fn partition(handle: &str) -> Action {
    Action::Partition {
        handle: handle.into(),
        left: vec!["a".into()],
        right: vec!["b".into()],
    }
}

fn plan(w: &mut World<Mock>, at: u64, action: Action) {
    w.faults
        .append(Plan {
            id: 0,
            at_ns: at,
            deadline_ns: at + 100,
            action,
        })
        .unwrap();
}

#[test]
fn rounds_end_at_the_window_and_the_duration_exactly() {
    let mut w = world(&["a", "b"]);
    assert_eq!(run(&mut w, 20, None).unwrap(), "duration");
    // latency 10, early 3: windows of 7, then the exact end.
    assert_eq!(
        trace(&w),
        [
            "a:advance:7",
            "b:advance:7",
            "a:advance:14",
            "b:advance:14",
            "a:advance:20",
            "b:advance:20"
        ]
    );
    assert_eq!(w.slices, 3);
}

#[test]
fn strict_delivery_is_never_early_and_equal_time_keeps_insertion_order() {
    let mut w = world(&["a"]);
    w.machines[0].common.time = 10;
    w.strict_events[0].insert((11, 1), b"strict".to_vec());
    w.schedule(0, 12, Payload::Serial(b"first".to_vec()));
    w.schedule(0, 12, Payload::Serial(b"second".to_vec()));
    assert_eq!(run(&mut w, 30, None).unwrap(), "duration");
    let t = trace(&w);
    // The management byte ends a round exactly at 11 and is delivered there;
    // serial input behind it is delivered early, in order, right after.
    assert_eq!(
        &t[..4],
        &[
            "a:advance:11",
            "a:control/strict:11",
            "a:serial/first:11",
            "a:serial/second:11"
        ]
    );
    assert!(w.strict_events[0].is_empty() && w.events[0].is_empty());
}

#[test]
fn control_then_trigger_then_fault_at_equal_time() {
    let mut w = world(&["a", "b"]);
    configure_control(&mut w);
    plan(&mut w, 50, partition("scheduled"));
    w.control
        .triggers
        .arm("a".into(), b"X".to_vec(), 0, 200, partition("trigger"))
        .unwrap();
    w.control.triggers.committed("a", 0, b'X', 50, 0).unwrap();
    w.control
        .request(
            0,
            0,
            b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"inspect\"}",
            0,
        )
        .unwrap();
    assert_eq!(
        segment(
            &mut w,
            &json!({"control":{"placement":{}}}),
            120,
            None,
            None,
            None,
            true
        )
        .unwrap(),
        "duration"
    );
    // Inspect at t=50 sees neither effect; both faults then run at t=50.
    assert_eq!(
        w.control.log[0]["response"]["result"]["active_handles"],
        json!([])
    );
    assert_eq!(w.control.log[0]["delivered_at_ns"], 100);
    assert_eq!(w.control.triggers.records[0].fault_id, Some(2));
    assert_eq!(w.faults.active_handles(), ["scheduled", "trigger"]);
    assert!(w.faults.log.iter().all(|row| row["applied_at_ns"] == 50));
    assert!(trace(&w)
        .iter()
        .any(|l| l.starts_with("a:control/SBX_REPLY1 1 1 ") && l.ends_with(":100")));
}

#[test]
fn equal_time_control_allocates_fault_before_trigger() {
    let mut w = world(&["a", "b", "c"]);
    configure_control(&mut w);
    plan(&mut w, 50, partition("scheduled"));
    w.control
        .triggers
        .arm("a".into(), b"X".to_vec(), 0, 200, partition("trigger"))
        .unwrap();
    w.control.triggers.committed("a", 0, b'X', 50, 0).unwrap();
    let request = json!({"version":1,"id":1,"op":"partition","left":[{"machine":"b"}],"right":[{"machine":"c"}]});
    w.control
        .request(0, 0, format!("SBX_RPC1 1 1 {request}").as_bytes(), 0)
        .unwrap();
    assert_eq!(run(&mut w, 120, None).unwrap(), "duration");
    assert_eq!(w.control.log[0]["fault_id"], 2);
    assert_eq!(w.control.triggers.records[0].fault_id, Some(3));
    assert_eq!(w.faults.log.len(), 3);
    assert!(w.faults.log.iter().all(|row| row["applied_at_ns"] == 50));
}

#[test]
fn requests_console_and_readiness_from_a_round_arm_later_boundaries() {
    let mut w = world(&["a", "b"]);
    configure_control(&mut w);
    w.control
        .triggers
        .arm("a".into(), b"X".to_vec(), 0, 200, partition("trigger"))
        .unwrap();
    plan(
        &mut w,
        30,
        Action::Stop {
            machine: "b".into(),
        },
    );
    let mut console = Step::new(2, Stop::Control);
    console.console = Some(b'X');
    let mut request = Step::new(4, Stop::Control);
    request
        .requests
        .push(b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}".to_vec());
    w.machines[0].steps.extend([console, request]);
    let mut ready = Step::new(3, Stop::Control);
    ready.effects.push((0, true));
    let mut done = Step::new(50, Stop::Control);
    done.effects.push((1, true));
    w.machines[1].steps.extend([ready, done]);
    assert_eq!(run(&mut w, 120, None).unwrap(), "duration");
    // The trigger matched at 2 and fired at 2 + 50; the request at 4 was
    // admitted at 4 + 50 and answered at 104; readiness at 3 let the stop
    // command go out at 30 and succeed at 50.
    let records = &w.control.triggers.records;
    assert_eq!(
        (
            records[0].matched_at_ns,
            records[0].effect_at_ns,
            records[0].status.as_str()
        ),
        (Some(2), Some(52), "fired")
    );
    assert_eq!(w.faults.active_handles(), ["trigger"]);
    assert_eq!(w.faults.readiness, [json!({"machine":"b","at_ns":3})]);
    assert_eq!(w.faults.log[0]["status"], "applied");
    assert_eq!(w.faults.log[0]["applied_at_ns"], 50);
    assert!(trace(&w).contains(&"b:control/stop 1\n:30".to_string()));
    assert_eq!(w.control.log.len(), 1);
    assert_eq!(w.control.log[0]["delivered_at_ns"], 104);
}

#[test]
fn pending_error_still_delivers_strict_reply_before_stopping() {
    let mut w = world(&["a"]);
    configure_control(&mut w);
    w.control
        .request(
            0,
            0,
            b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}",
            0,
        )
        .unwrap();
    w.control.pending.as_mut().unwrap().fatal = true;
    w.control.pending.as_mut().unwrap().error = Some("effect failed".into());
    assert_eq!(run(&mut w, 150, None).unwrap(), "fault_failed");
    assert_eq!(w.control.log[0]["delivered_at_ns"], 100);
    assert!(!w.control.has_undelivered_reply());
    assert!(trace(&w)
        .iter()
        .any(|line| line.starts_with("a:control/SBX_REPLY1 ")));
}

#[test]
fn time_fork_settles_owed_io_before_parking_and_snapshotting() {
    let mut w = world(&["a"]);
    w.machines[0].owed = Some(20);
    assert_eq!(
        segment(&mut w, &Value::Null, 100, None, Some(Some(10)), None, true).unwrap(),
        "barrier"
    );
    assert_eq!(w.machines[0].time, 20);
    assert!(w.machines[0].parked);
    assert!(w.machines[0].owed.is_none());
    // Windows of 7, the exact fork boundary, then the owed I/O to 20.
    assert_eq!(
        trace(&w),
        [
            "a:advance:7",
            "a:advance:10",
            "a:advance:17",
            "a:advance:20",
            "a:park:20"
        ]
    );
    let snapshot = WorldSnapshot::take(&mut w).unwrap();
    w.machines[0].common.time = 70;
    w.machines[0].common.parked = false;
    w.schedule(0, 90, Payload::Serial(vec![1]));
    assert_eq!(snapshot.restore(&mut w).unwrap(), 1);
    assert_eq!(w.machines[0].time, 20);
    assert!(w.machines[0].parked);
    assert!(w.events[0].is_empty());
}

#[test]
fn owed_io_is_split_at_strict_boundary_before_time_fork() {
    let mut w = world(&["a"]);
    w.machines[0].owed = Some(20);
    w.strict_events[0].insert((7, 1), b"strict".to_vec());
    assert_eq!(
        segment(&mut w, &Value::Null, 100, None, Some(Some(10)), None, true).unwrap(),
        "barrier"
    );
    assert_eq!(
        trace(&w),
        [
            "a:advance:7",
            "a:control/strict:7",
            "a:advance:10",
            "a:advance:17",
            "a:advance:20",
            "a:park:20"
        ]
    );
    assert!(w.machines[0].owed.is_none());
}

#[test]
fn idle_machines_park_at_the_fork_time_without_waking() {
    let mut w = world(&["a", "b"]);
    w.machines[0].steps.push_back(Step::new(3, Stop::Halted));
    w.machines[0].timer = 500;
    assert_eq!(
        segment(&mut w, &Value::Null, 100, None, Some(Some(10)), None, true).unwrap(),
        "barrier"
    );
    assert_eq!(machine_trace(&w, "a"), ["a:advance:3", "a:park:3"]);
    assert_eq!(
        machine_trace(&w, "b"),
        ["b:advance:7", "b:advance:10", "b:park:10"]
    );
    let mut exits = world(&["a", "b"]);
    exits.machines[0]
        .steps
        .push_back(Step::new(3, Stop::Exited));
    assert_eq!(
        segment(
            &mut exits,
            &Value::Null,
            100,
            None,
            Some(Some(10)),
            None,
            true
        )
        .unwrap_err(),
        "a exited before the fork point"
    );
}

#[test]
fn initial_launch_deadline_is_an_ordinary_outcome_but_fork_still_requires_barrier() {
    let mut w = world(&["a", "b"]);
    w.machines[0].steps.push_back(Step::new(2, Stop::Barrier));
    assert_eq!(
        segment(&mut w, &Value::Null, 7, None, Some(None), None, false).unwrap(),
        "duration"
    );
    assert!(w.machines[0].parked);
    assert_eq!(w.machines[1].time, 7);
    assert!(trace(&w).iter().all(|s| !s.contains(":release:")));
    let mut strict = world(&["a"]);
    assert!(
        segment(&mut strict, &Value::Null, 7, None, Some(None), None, true)
            .unwrap_err()
            .contains("launch barrier not reached: duration")
    );
}

#[test]
fn initial_launch_preserves_end_machine_exit_and_cancellation() {
    let mut w = world(&["a", "b"]);
    w.machines[0].steps.push_back(Step::new(3, Stop::Exited));
    assert_eq!(
        segment(&mut w, &Value::Null, 20, Some("a"), Some(None), None, false).unwrap(),
        "end_machine_exited"
    );
    assert_eq!(w.machines[0].state, State::Exited(0));
    let mut cancelled = world(&["a"]);
    let run = Run {
        cfg: &Value::Null,
        duration: 20,
        end_on: &None,
        fork: Some(None),
        checkpoint: None,
        require_barrier: false,
    };
    assert_eq!(
        simulate(&mut cancelled, &run, Duration::ZERO, &mut || Err(
            "cancelled".into()
        ))
        .unwrap_err(),
        "cancelled"
    );
    assert!(trace(&cancelled).is_empty());
    assert_eq!(cancelled.slices, 0);
}

#[test]
fn initial_barrier_release_precedes_first_packet_rpc_and_trigger_effects() {
    let names = vec!["a".to_owned(), "b".to_owned()];
    let cfg = json!({"control":{"machine":"a","latency_ns":10,"placement":{}}});
    for seed in [1009, 1010, 1011] {
        for skew in [0, 10] {
            let mut w = world(&["a", "b"]);
            w.barrier = true;
            w.early = 0;
            w.machines[0].steps.push_back(Step::new(2, Stop::Barrier));
            w.machines[1].steps.push_back(Step::new(3, Stop::Barrier));
            assert_eq!(
                segment(&mut w, &cfg, 100, None, Some(None), None, false).unwrap(),
                "barrier"
            );
            assert_eq!(w.slices, 1);
            let (kills, reseed) = release(&mut w, &cfg, &names, seed, skew, &mut |_, _| {
                panic!("unsupported")
            })
            .unwrap();
            assert!(kills.is_empty());
            assert_eq!(reseed, json!({"a":"barrier","b":"barrier"}));
            let upper = 3 + skew;
            for m in &w.machines {
                assert!(!m.parked);
                assert!((3..=upper).contains(&m.time));
            }
            // The released world runs on from where it stands.
            assert_eq!(
                segment(&mut w, &cfg, 100, None, None, None, true).unwrap(),
                "duration"
            );
            assert!(w.machines.iter().all(|m| m.time == 100));
        }
    }
}

#[test]
fn a_crash_discards_management_and_a_reboot_drops_earlier_frames() {
    let mut w = world(&["a", "b"]);
    plan(
        &mut w,
        6,
        Action::Crash {
            machine: "a".into(),
        },
    );
    plan(
        &mut w,
        10,
        Action::Reboot {
            machine: "a".into(),
        },
    );
    w.machines[0].owed = Some(7);
    w.strict_events[0].insert((8, 1), b"old".to_vec());
    for at in [9, 11] {
        w.schedule(
            0,
            at,
            Payload::Frame {
                from: 1,
                bytes: vec![0; 14],
            },
        );
    }
    assert_eq!(run(&mut w, 30, None).unwrap(), "duration");
    assert_eq!(w.machines[0].incarnation, 1);
    assert!(!w.machines[0].down);
    assert_eq!(w.machines[0].booted_at, 10);
    assert_eq!(w.incarnations[0][0]["crashed_at_ns"], 6);
    assert_eq!(w.incarnations[0][0]["discarded_pending_io"], true);
    assert_eq!(w.incarnations[0][0]["discarded_management_bytes"], 3);
    assert!(w.strict_events[0].is_empty());
    assert_eq!((w.dropped, w.delivered), (1, 1));
    assert_eq!(
        w.network_snapshot(),
        json!({"b->a":{"delivered":1,"partition_dropped":0,"receiver_dropped":1}})
    );
    let a = machine_trace(&w, "a");
    assert_eq!(a[0], "a:advance:6");
    assert!(!a.contains(&"a:control/old:8".to_string()));
    assert!(a.contains(&"a:frame:10".to_string()), "{a:?}");
    assert!(w.faults.log.iter().all(|row| row["status"] == "applied"));
}

#[test]
fn a_paused_machine_holds_its_frames_and_pays_owed_io_after_resume() {
    let mut w = world(&["a", "b"]);
    plan(
        &mut w,
        5,
        Action::Pause {
            machine: "a".into(),
        },
    );
    plan(
        &mut w,
        20,
        Action::Resume {
            machine: "a".into(),
        },
    );
    w.machines[0].owed = Some(7);
    // b sends to a at 7: delivered at 17 while a is paused, so held.
    let mut send = Step::new(7, Stop::Sent);
    let mut frame = vec![0; 14];
    frame[5] = 0;
    send.frames.push(frame);
    w.machines[1].steps.push_back(send);
    assert_eq!(run(&mut w, 30, None).unwrap(), "duration");
    let a = machine_trace(&w, "a");
    assert_eq!(a[..2], ["a:advance:5", "a:pause:5"]);
    assert_eq!(a[2], "a:resume:20");
    assert!(a[3..].contains(&"a:frame:20".to_string()), "{a:?}");
    assert_eq!(w.paused_ingress[0], 1);
    assert_eq!(w.delivered, 1);
    assert_eq!(w.machines[0].time, 30);
    assert!(w.machines[0].owed.is_none());
}

#[test]
fn a_world_with_only_paused_machines_ends_at_the_duration() {
    let mut w = world(&["a"]);
    plan(
        &mut w,
        5,
        Action::Pause {
            machine: "a".into(),
        },
    );
    assert_eq!(run(&mut w, 30, None).unwrap(), "duration");
    assert_eq!(w.machines[0].time, 5);
    let mut idle = world(&["a"]);
    idle.machines[0].steps.push_back(Step::new(3, Stop::Halted));
    assert_eq!(run(&mut idle, 30, None).unwrap(), "quiescent");
}

#[test]
fn runtime_checkpoint_precedes_exact_time_fault_and_preserves_active_plan() {
    let mut w = world(&["a", "b"]);
    plan(&mut w, 10, partition("checkpoint-cut"));
    assert_eq!(
        segment(&mut w, &Value::Null, 30, None, None, Some(10), true).unwrap(),
        "checkpoint"
    );
    assert!(w.machines.iter().all(|m| m.time == 10));
    assert_eq!(w.faults.log[0]["status"], "unrequested");
    assert!(!w.faults.blocked(0, 1));
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    for _ in 0..2 {
        saved.restore_checkpoint(&mut w).unwrap();
        assert_eq!(
            segment(&mut w, &Value::Null, 30, None, None, Some(11), true).unwrap(),
            "checkpoint"
        );
        assert!(w.faults.blocked(0, 1));
        assert_eq!(w.faults.log.len(), 1);
        assert!(w.machines.iter().all(|m| m.time == 11));
    }
}

#[test]
fn runtime_checkpoint_absorbs_committed_messages_before_selection_and_snapshot() {
    let mut w = world(&["a"]);
    configure_control(&mut w);
    w.machines[0].time = 10;
    w.machines[0].fault_effects.push((0, 9, true));
    w.machines[0].control_requests.push((
        9,
        b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}".to_vec(),
    ));
    assert!(
        matches!(WorldSnapshot::take_checkpoint(&mut w), Err(error) if error.contains("drained"))
    );
    assert_eq!(
        segment(&mut w, &Value::Null, 30, None, None, Some(10), true).unwrap(),
        "checkpoint"
    );
    assert!(w.machines[0].fault_effects.is_empty());
    assert!(w.machines[0].control_requests.is_empty());
    assert_eq!(w.faults.readiness, [json!({"machine":"a", "at_ns":9})]);
    let boundary = w.control.boundary();
    assert_eq!(boundary, 59);
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    saved.restore_checkpoint(&mut w).unwrap();
    assert!(w.control.pending.is_some());
    assert_eq!(w.control.boundary(), boundary);
    let names = vec!["a".into()];
    reseed_checkpoint(&mut w, &names, 2, 3, "branch").unwrap();
    assert_eq!(
        segment(&mut w, &Value::Null, 200, None, None, None, true).unwrap(),
        "duration"
    );
    assert_eq!(w.control.log[0]["delivered_at_ns"], 109);
}

#[test]
fn trigger_partial_match_and_scheduled_effect_survive_snapshot_restore() {
    for marker in [b"AB".to_vec(), b"A".to_vec()] {
        let mut w = world(&["a", "b"]);
        configure_control(&mut w);
        w.control
            .triggers
            .arm("a".into(), marker, 0, 200, partition("trigger"))
            .unwrap();
        let mut first = Step::new(2, Stop::Control);
        first.console = Some(b'A');
        w.machines[0].steps.push_back(first);
        let mut second = Step::new(6, Stop::Control);
        second.console = Some(b'B');
        w.machines[0].steps.push_back(second);
        assert_eq!(
            segment(&mut w, &Value::Null, 150, None, Some(Some(4)), None, true).unwrap(),
            "barrier"
        );
        assert_eq!(w.control.triggers.records[0].seen, 1);
        let snapshot = WorldSnapshot::take(&mut w).unwrap();
        let mut outcomes = vec![];
        for pass in 0..2 {
            if pass == 1 {
                snapshot.restore(&mut w).unwrap();
            }
            for m in &mut w.machines {
                m.release(4, [0; 8]).unwrap();
            }
            run(&mut w, 100, None).unwrap();
            outcomes.push((
                w.control.triggers.evidence().unwrap(),
                w.faults.log.clone(),
                w.faults.active_handles(),
            ));
        }
        assert_eq!(outcomes[0], outcomes[1]);
        assert_eq!(outcomes[0].2, ["trigger"]);
    }
}

#[test]
fn adapter_observations_are_rejected_before_effects_are_consumed() {
    for bad in [
        "past",
        "future",
        "identity",
        "incarnation",
        "lifecycle",
        "state",
        "console",
        "request",
        "effect",
        "order",
    ] {
        let mut w = world(&["a", "b"]);
        configure_control(&mut w);
        w.machines[0].common.time = 2;
        w.machines[1].common.time = 2;
        w.control
            .triggers
            .arm("a".into(), b"X".to_vec(), 0, 200, partition("trigger"))
            .unwrap();
        let mut step = Step::new(5, Stop::Control);
        step.console = Some(b'X');
        step.effects = vec![(0, true)];
        step.requests = vec![b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}".to_vec()];
        step.frames = vec![vec![255; 14]];
        w.machines[0].steps.push_back(step);
        w.machines[0].bad = Some(bad);
        let error = run(&mut w, 100, None).unwrap_err();
        assert!(
            error.contains("invalid stopped observation"),
            "{bad}: {error}"
        );
        assert_eq!(w.control.triggers.records[0].seen, 0, "{bad}");
        assert!(w.faults.readiness.is_empty(), "{bad}");
        assert!(w.control.pending.is_none(), "{bad}");
        assert!(w.events.iter().all(BTreeMap::is_empty), "{bad}");
        assert_eq!(w.delivered + w.dropped, 0, "{bad}");
        assert_eq!(w.slices, 0, "{bad}");
    }
}

#[test]
fn a_failed_crash_terminates_every_worker_and_poisons_the_world() {
    let mut w = world(&["a", "b"]);
    plan(
        &mut w,
        5,
        Action::Crash {
            machine: "a".into(),
        },
    );
    w.factory.lock().unwrap().push("fail/timer".into());
    let error = run(&mut w, 30, None).unwrap_err();
    assert!(error.contains("timer"), "{error}");
    assert!(w.failure.is_some());
    let terminations = trace(&w)
        .iter()
        .filter(|x| x.contains(":terminate:"))
        .count();
    assert_eq!(terminations, 3);
    assert!(run(&mut w, 30, None).unwrap_err().contains("terminal"));
}

/// The legacy receipts' world: pause, crash, resume and reboot faults, a
/// console trigger, readiness, a control request, serial input and a frame.
fn scenario(variant: u64) -> World<Mock> {
    let mut w = world(&["a", "b", "c"]);
    configure_control(&mut w);
    w.run_seed = variant + 1;
    w.early = variant % 4;
    w.control.latency = 60;
    w.machines[1].owed = Some(13 + variant);
    w.schedule(2, 19, Payload::Serial(b"input".to_vec()));
    w.schedule(2, 19, Payload::Serial(b"second".to_vec()));
    w.schedule(
        0,
        25,
        Payload::Partition {
            from: 0,
            to: 1,
            on: true,
        },
    );
    w.control
        .triggers
        .arm("a".into(), b"A".to_vec(), 0, 200, partition("trigger"))
        .unwrap();
    for (at, action) in [
        (
            18,
            Action::Pause {
                machine: "c".into(),
            },
        ),
        (
            35,
            Action::Crash {
                machine: "b".into(),
            },
        ),
        (
            44,
            Action::Resume {
                machine: "c".into(),
            },
        ),
        (
            55,
            Action::Reboot {
                machine: "b".into(),
            },
        ),
    ] {
        plan(&mut w, at, action);
    }
    let mut console = Step::new(2, Stop::Control);
    console.console = Some(b'A');
    let mut ready = Step::new(3, Stop::Control);
    ready.effects.push((0, true));
    let mut request = Step::new(4, Stop::Control);
    request
        .requests
        .push(b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}".to_vec());
    let mut send = Step::new(20 + variant, Stop::Sent);
    let mut frame = vec![0; 14];
    frame[5] = 1;
    send.frames.push(frame.clone());
    let mut late = Step::new(30 + variant, Stop::Sent);
    late.frames.push(frame);
    w.machines[0].steps.extend([
        console,
        ready,
        request,
        send,
        late,
        Step::new(150, Stop::Exited),
    ]);
    w.machines[2].steps.push_back(Step::new(12, Stop::Halted));
    w.machines[2].timer = 27;
    w
}

fn receipt(w: &World<Mock>, outcome: &str) -> Value {
    let machines=w.machines.iter().map(|m|json!({"name":m.name,"time":m.time,"state":format!("{:?}",m.state),"parked":m.parked,"paused":m.paused,"down":m.down,"incarnation":m.incarnation,"booted_at":m.booted_at,"owed":m.owed})).collect::<Vec<_>>();
    let events = w
        .events
        .iter()
        .map(|events| {
            events
                .iter()
                .map(|(key, value)| match value {
                    Payload::Frame { from, bytes } => json!([key, "frame", from, bytes]),
                    Payload::Serial(bytes) => json!([key, "serial", bytes]),
                    Payload::Partition { from, to, on } => {
                        json!([key, "partition", from, to, on])
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let traces: Vec<_> = w
        .machines
        .iter()
        .map(|m| machine_trace(w, &m.name))
        .collect();
    json!({"outcome":outcome,"machines":machines,"traces":traces,"events":events,"strict":w.strict_events.iter().map(|e|e.iter().collect::<Vec<_>>()).collect::<Vec<_>>(),"faults":w.faults.log,"readiness":w.faults.readiness,"control":w.control.log,"triggers":w.control.triggers.evidence().unwrap(),"incarnations":w.incarnations,"network":w.network_snapshot(),"slices":w.slices,"delivered":w.delivered,"dropped":w.dropped,"partitioned":w.partitioned.iter().collect::<Vec<_>>()})
}

#[test]
fn worker_count_and_completion_order_never_change_a_world_with_faults_and_control() {
    for variant in 0..8u64 {
        let mut reference = None;
        for (workers, skew) in [(0, 0), (1, 0), (2, 0), (3, 0), (3, 2_000), (2, 2_000)] {
            let mut w = scenario(variant);
            let (outcome, evidence) =
                run_with(&mut w, 160, Some("a"), workers, Duration::from_micros(skew)).unwrap();
            assert_eq!(outcome, "end_machine_exited", "variant {variant}");
            assert_eq!(w.delivered + w.dropped, 2, "variant {variant}");
            assert_eq!(w.machines[1].incarnation, 1);
            assert_eq!(w.control.log[0]["delivered_at_ns"], 4 + 60 + 60);
            assert_eq!(w.control.triggers.records[0].status, "fired");
            assert!(
                w.faults.log.iter().all(|row| row["status"] == "applied"),
                "{:?}",
                w.faults.log
            );
            let got = (receipt(&w, outcome), evidence["effects_digest"].clone());
            match &reference {
                None => reference = Some(got),
                Some(want) => assert_eq!(
                    want, &got,
                    "variant {variant}, workers {workers}, skew {skew}"
                ),
            }
        }
    }
}

#[test]
fn a_fault_that_fails_ends_the_run_with_its_evidence() {
    let mut w = world(&["a", "b"]);
    plan(
        &mut w,
        5,
        Action::Stop {
            machine: "b".into(),
        },
    );
    let mut ready = Step::new(3, Stop::Control);
    ready.effects.push((0, true));
    let mut failed = Step::new(20, Stop::Control);
    failed.effects.push((1, false));
    w.machines[1].steps.extend([ready, failed]);
    assert_eq!(run(&mut w, 100, None).unwrap(), "fault_failed");
    assert_eq!(w.faults.log[0]["status"], "failed");
    assert!(w.faults.error.is_some());
    // The round holding the failed effect commits; nothing runs after it.
    assert!(w.machines.iter().all(|m| m.time == 26), "{:?}", trace(&w));
}

#[test]
fn an_event_already_due_is_delivered_even_when_the_machine_stands_past_the_slice_end() {
    // A skewed release can leave a machine past a queued event's time. Its
    // first round may assign it a slice it has already passed; the overdue
    // event is still delivered there, at the machine's time, in every state.
    for halted in [false, true] {
        let mut w = world(&["a", "b"]);
        // latency 10, early 3: round [21, 28) ends early at 25; `a` stands at 26.
        w.machines[0].common.time = 26;
        if halted {
            w.machines[0].common.state = State::Halted;
        }
        w.schedule(0, 12, Payload::Serial(b"late".to_vec()));
        assert_eq!(run(&mut w, 40, None).unwrap(), "duration");
        let t = machine_trace(&w, "a");
        assert_eq!(t[0], "a:serial/late:26", "{halted}: {t:?}");
        assert!(w.events[0].is_empty());
    }
}

#[test]
fn throttle_applies_at_its_boundary_survives_reboot_and_heals_identically_across_workers() {
    let mut reference = None;
    for workers in [0, 1, 2, 4] {
        let mut w = world(&["a", "b"]);
        for (at, action) in [
            (
                10,
                json!({"action":"throttle","handle":"t","machine":"b","factor_permille":4000}),
            ),
            (20, json!({"action":"crash","machine":"b"})),
            (25, json!({"action":"reboot","machine":"b"})),
            (30, json!({"action":"heal","handle":"t"})),
        ] {
            let mut action = action;
            action["id"] = json!(0);
            action["at_ns"] = json!(at);
            action["deadline_ns"] = json!(at);
            w.faults
                .append(serde_json::from_value(action).unwrap())
                .unwrap();
        }
        for at in [5, 15, 27, 35] {
            w.machines[1].steps.push_back(Step::new(at, Stop::Sent));
        }
        run_with(&mut w, 60, None, workers, Duration::from_micros(3)).unwrap();
        let throttles: Vec<String> = machine_trace(&w, "b")
            .into_iter()
            .filter(|line| line.contains(":throttle/"))
            .collect();
        // Each change lands with the machine at its exact boundary; the
        // crash's fresh incarnation is throttled again.
        assert_eq!(
            throttles,
            [
                "b:throttle/4000:10",
                "b:throttle/4000:20",
                "b:throttle/1000:30"
            ]
        );
        assert!(w.faults.active_handles().is_empty());
        assert!(w.faults.log.iter().all(|row| row["status"] == "applied"));
        let observed = (throttles, w.faults.log.clone());
        if let Some(want) = &reference {
            assert_eq!(&observed, want);
        } else {
            reference = Some(observed);
        }
    }
}

#[test]
fn throttle_of_an_unsupported_machine_is_a_recoverable_unsupported_error() {
    let mut w = world(&["a", "b"]);
    configure_control(&mut w);
    w.machines[1].throttleable = false;
    let request =
        json!({"version":1,"id":1,"op":"throttle","target":{"machine":"b"},"factor_permille":2000});
    w.control
        .request(0, 0, format!("SBX_RPC1 1 1 {request}").as_bytes(), 0)
        .unwrap();
    assert_eq!(run(&mut w, 120, None).unwrap(), "duration");
    let response = &w.control.log[0]["response"];
    assert_eq!(response["error"]["code"], "unsupported", "{response}");
    assert!(w.faults.log.is_empty());
    assert!(w.control.error.is_none());
    assert!(!machine_trace(&w, "b")
        .iter()
        .any(|l| l.contains("throttle")));
}
