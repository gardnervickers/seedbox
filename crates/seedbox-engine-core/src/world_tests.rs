use super::*;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

type Trace = Rc<RefCell<Vec<String>>>;
#[derive(Clone)]
struct Step {
    at: u64,
    stop: Stop,
    console: Option<u8>,
    requests: Vec<Vec<u8>>,
    effects: Vec<(u32, bool)>,
    frames: Vec<Vec<u8>>,
}
impl Step {
    fn new(at: u64, stop: Stop) -> Self {
        Self {
            at,
            stop,
            console: None,
            requests: vec![],
            effects: vec![],
            frames: vec![],
        }
    }
}
struct Mock {
    common: MachineState,
    trace: Trace,
    steps: VecDeque<Step>,
    owed: Option<u64>,
    irq: bool,
    timer: u64,
    bad: Option<&'static str>,
}
#[derive(Clone)]
struct Saved {
    common: MachineState,
    steps: VecDeque<Step>,
    irq: bool,
    timer: u64,
}
impl Deref for Mock {
    type Target = MachineState;
    fn deref(&self) -> &MachineState {
        &self.common
    }
}
impl DerefMut for Mock {
    fn deref_mut(&mut self) -> &mut MachineState {
        &mut self.common
    }
}
impl Mock {
    fn new(name: &str, trace: Trace) -> Self {
        Self {
            common: MachineState::new(name.into()),
            trace,
            steps: VecDeque::new(),
            owed: None,
            irq: false,
            timer: u64::MAX,
            bad: None,
        }
    }
    fn record(&self, kind: &str) {
        self.trace
            .borrow_mut()
            .push(format!("{}:{kind}:{}", self.name, self.time));
    }
}
impl WorldMachine for Mock {
    type Factory = Trace;
    type Boot = ();
    type Clock = ();
    type Snapshot = Saved;
    fn boot(trace: &Trace, name: &str, _: &(), _: bool, _: [u8; 32]) -> Result<Self, String> {
        if trace.borrow().iter().any(|x| x == "fail/boot") {
            trace.borrow_mut().push("partial-boot-cleanup".into());
            return Err("boot failed".into());
        }
        let mut machine = Self::new(name, trace.clone());
        if trace.borrow().iter().any(|x| x == "fail/timer") {
            machine.bad = Some("timer");
        }
        if trace.borrow().iter().any(|x| x == "fail/install") {
            machine.bad = Some("install");
        }
        Ok(machine)
    }
    fn has_pending_irq(&self) -> bool {
        self.irq
    }
    fn next_deadline(&self) -> u64 {
        self.timer
    }
    fn owed_io(&self) -> Option<u64> {
        self.owed
    }
    fn has_control(&self) -> bool {
        true
    }
    fn deliver_control(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.record(&format!("control/{}", String::from_utf8_lossy(bytes)));
        Ok(())
    }
    fn deliver_serial(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.record(&format!("serial/{}", String::from_utf8_lossy(bytes)));
        Ok(())
    }
    fn deliver_frame(&mut self, _: &[u8]) -> Result<bool, String> {
        self.record("frame");
        Ok(true)
    }
    fn pause(&mut self) -> Result<(), String> {
        self.common.pause()?;
        self.record("pause");
        Ok(())
    }
    fn resume(&mut self, at: u64) -> Result<(), String> {
        self.common.resume(at)?;
        self.record("resume");
        Ok(())
    }
    fn boot_at(&mut self, at: u64) -> Result<(), String> {
        self.common.boot_at(at)?;
        Ok(())
    }
    fn succeed(&mut self, old: &mut Self, at: u64, _: u64) -> Result<Option<u64>, String> {
        if self.bad == Some("install") {
            return Err("install ACK lost".into());
        }
        self.common.succeed(&old.common, at)?;
        Ok(Some(0))
    }
    fn park(&mut self) -> Result<(), String> {
        self.common.check_park(self.owed.is_some())?;
        self.common.commit_park();
        self.record("park");
        Ok(())
    }
    fn wake(&mut self, at: u64) -> Result<(), String> {
        self.common.time = self.common.time.max(at);
        self.common.wake();
        self.record("wake");
        Ok(())
    }
    fn release(&mut self, at: u64, _: [u32; 8]) -> Result<Reseed, String> {
        self.common.begin_release()?;
        self.common.time = self.common.time.max(at);
        self.record("release");
        Ok(Reseed::Barrier)
    }
    fn abi(&self) -> u64 {
        3
    }
    fn seed_timer(&mut self, _: u64, _: &str) -> Result<(), String> {
        if self.bad == Some("timer") {
            Err("timer ACK lost".into())
        } else {
            Ok(())
        }
    }
    fn checkpoint_boundary(_: &()) -> Result<(), String> {
        Ok(())
    }
    fn restore_boundary(_: &mut ()) -> Result<(), String> {
        Ok(())
    }
    fn seed_timer_branch(
        &mut self,
        seed: u64,
        identity: Option<&str>,
        domain: &str,
    ) -> Result<(), String> {
        if let Some(identity) = identity {
            self.record(&format!("branch/{seed}/{identity}/{domain}"));
        }
        self.seed_timer(seed, domain)
    }
    fn reseed_guest(&mut self, _: [u32; 8]) -> Result<(), String> {
        self.record("reseed-guest");
        Ok(())
    }
    fn supports_speed_reseed(&self) -> bool {
        true
    }
    fn seed_speed(&mut self, _: u64, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn branches(&self) -> u64 {
        0
    }
    fn exits(&self) -> u64 {
        0
    }
    fn exit_witness(&mut self) -> Result<String, String> {
        if self.bad == Some("witness") {
            return Err("witness ACK lost".into());
        }
        Ok(String::new())
    }
    fn terminate_worker(&mut self) -> Result<(), String> {
        self.record("terminate");
        Ok(())
    }
    fn snapshot(&mut self) -> Result<Saved, String> {
        if self.owed.is_some() {
            return Err("owed I/O".into());
        }
        Ok(Saved {
            common: self.common.clone(),
            steps: self.steps.clone(),
            irq: self.irq,
            timer: self.timer,
        })
    }
    fn restore(&mut self, s: &Saved) -> Result<u64, String> {
        self.common = s.common.clone();
        self.steps = s.steps.clone();
        self.owed = None;
        self.irq = s.irq;
        self.timer = s.timer;
        self.common.clear_effects();
        Ok(1)
    }
    fn advance(
        &mut self,
        _: &mut (),
        request: SliceRequest,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        assert!(request.hard_limit >= self.time);
        assert_eq!(request.observe_console, self.observe_console);
        let step = if self
            .steps
            .front()
            .is_some_and(|s| s.at <= request.hard_limit)
        {
            self.steps.pop_front().unwrap()
        } else {
            Step::new(request.hard_limit, Stop::Reached)
        };
        let paid = step.at.saturating_sub(self.time);
        self.owed = self
            .owed
            .and_then(|owed| owed.checked_sub(paid))
            .filter(|&owed| owed > 0);
        self.common.time = step.at;
        self.common.committed_console = step
            .console
            .filter(|_| request.observe_console)
            .map(|byte| (step.at, byte));
        self.common
            .control_requests
            .extend(step.requests.into_iter().map(|bytes| (step.at, bytes)));
        self.common
            .fault_effects
            .extend(step.effects.into_iter().map(|(id, ok)| (id, step.at, ok)));
        frames.extend(step.frames);
        match self.bad.take() {
            Some("past") => self.common.time = paid.saturating_sub(paid + 1),
            Some("future") => self.common.time = request.hard_limit + 1,
            Some("identity") => self.common.name = "wrong".into(),
            Some("incarnation") => self.common.incarnation += 1,
            Some("lifecycle") => self.common.paused = true,
            Some("state") => self.common.state = State::Halted,
            Some("console") => self.common.committed_console = Some((request.hard_limit + 1, b'X')),
            Some("request") => self
                .common
                .control_requests
                .push((request.hard_limit + 1, b"bad".to_vec())),
            Some("effect") => self
                .common
                .fault_effects
                .push((0, request.hard_limit + 1, true)),
            Some("order") => self
                .common
                .fault_effects
                .extend([(0, step.at, true), (0, 0, true)]),
            None => (),
            Some(other) => panic!("unknown test mutation {other}"),
        }
        match step.stop {
            Stop::Exited => self.common.state = State::Exited(0),
            Stop::Halted => self.common.state = State::Halted,
            _ => (),
        }
        self.record("advance");
        Ok(step.stop)
    }
}
fn world(names: &[&str]) -> World<Mock> {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let trace = Trace::default();
    let n = names.len();
    World {
        failure: None,
        machines: names
            .iter()
            .map(|name| Mock::new(name, trace.clone()))
            .collect(),
        factory: trace,
        boot: vec![(); n],
        barrier: false,
        boot_seed: 1,
        run_seed: 2,
        branch_identity: None,
        incarnations: vec![vec![]; n],
        paused_ingress: vec![0; n],
        clock: (),
        events: vec![BTreeMap::new(); n],
        seq: 0,
        lookahead: 10,
        early: 3,
        latency: 10,
        partitioned: BTreeMap::new(),
        faults: boot_faults(
            &names,
            &[],
            10,
            links::Links::from_config(1, &names, &json!({"latency_ns":10})).unwrap(),
        )
        .unwrap(),
        control: control::Control::new(&Value::Null, names, 10, 3).unwrap(),
        strict_events: vec![BTreeMap::new(); n],
        macs: (0..n).map(|i| [0, 0, 0, 0, 0, i as u8]).collect(),
        dropped: 0,
        delivered: 0,
        link_counts: BTreeMap::new(),
        semantic_counts: network_counters::NetworkCounters::default(),
        slices: 0,
    }
}
#[test]
fn tie_break_and_halted_wake_keys_use_one_coordinator() {
    let mut w = world(&["a", "b"]);
    assert_eq!(
        simulate(&mut w, &Value::Null, 20, &None, None, &mut || Ok(())).unwrap(),
        "duration"
    );
    assert_eq!(
        &*w.factory.borrow(),
        &["a:advance:10", "b:advance:20", "a:advance:20"]
    );
    w.machines[0].common.state = State::Halted;
    w.machines[0].timer = 80;
    w.schedule(0, 50, Payload::Serial(vec![]));
    assert_eq!(w.key(0), 50);
    w.machines[0].irq = true;
    assert_eq!(w.key(0), 20);
    w.machines[0].common.paused = true;
    assert_eq!(w.key(0), u64::MAX);
}
#[test]
fn strict_delivery_is_never_early_and_equal_time_keeps_insertion_order() {
    let mut w = world(&["a"]);
    w.machines[0].common.time = 10;
    w.strict_events[0].insert((11, 1), b"strict".to_vec());
    w.schedule(0, 12, Payload::Serial(b"first".to_vec()));
    w.schedule(0, 12, Payload::Serial(b"second".to_vec()));
    w.deliver_due(0).unwrap();
    assert_eq!(
        &*w.factory.borrow(),
        &["a:serial/first:10", "a:serial/second:10"]
    );
    assert_eq!(w.strict_events[0].len(), 1);
    w.machines[0].common.time = 11;
    w.deliver_due(0).unwrap();
    assert_eq!(w.factory.borrow().last().unwrap(), "a:control/strict:11");
}
#[test]
fn terminal_evidence_cuts_off_armed_and_matched_triggers_at_the_exiting_target() {
    let mut w = world(&["scenario", "peer"]);
    w.machines[0].time = 10;
    w.machines[0].state = State::Exited(0);
    w.machines[1].time = 20;
    let action = seedbox_control::Action::Stop {
        machine: "peer".into(),
    };
    w.control
        .triggers
        .arm("peer".into(), b"NEVER".to_vec(), 0, 100, action.clone())
        .unwrap();
    w.control
        .triggers
        .arm("peer".into(), b"M".to_vec(), 0, 100, action)
        .unwrap();
    w.control.triggers.committed("peer", 8, b'M', 5, 8).unwrap();
    assert_eq!(w.control.triggers.records[1].status, "matched");
    assert_eq!(
        w.finish("end_machine_exited", 50, &Some("scenario".into()))
            .unwrap(),
        10
    );
    for record in &w.control.triggers.records {
        assert_eq!(record.status, "cut_off");
        assert_eq!(record.cut_off_at_ns, Some(10));
    }
    assert_eq!(w.control.triggers.records[1].effect_at_ns, Some(13));
    assert_eq!(w.finish("duration", 50, &None).unwrap(), 50);
    assert_eq!(w.finish("all_exited", 50, &None).unwrap(), 20);
}
#[test]
fn duration_finalization_preserves_completed_trigger_records() {
    let mut w = world(&["a"]);
    let action = seedbox_control::Action::Stop {
        machine: "a".into(),
    };
    for (marker, timeout) in [(b'A', 100), (b'C', 100), (b'E', 3), (b'F', 100)] {
        w.control
            .triggers
            .arm("a".into(), vec![marker], 0, timeout, action.clone())
            .unwrap();
    }
    w.control.triggers.cancel(2, 2).unwrap();
    w.control.triggers.apply_boundary(&mut w.faults, 3).unwrap();
    w.control.triggers.committed("a", 4, b'F', 1, 4).unwrap();
    w.control.triggers.apply_boundary(&mut w.faults, 5).unwrap();
    let completed = w.control.triggers.records[1..].to_vec();
    assert_eq!(
        completed
            .iter()
            .map(|r| r.status.as_str())
            .collect::<Vec<_>>(),
        ["cancelled", "expired", "fired"]
    );
    assert_eq!(w.finish("duration", 9, &None).unwrap(), 9);
    assert_eq!(w.control.triggers.records[0].status, "cut_off");
    assert_eq!(w.control.triggers.records[0].cut_off_at_ns, Some(9));
    assert_eq!(w.control.triggers.records[1..], completed);
}
#[test]
fn failed_crash_immediately_terminates_workers_and_retained_world_cannot_retry() {
    for phase in ["boot", "timer", "witness", "install"] {
        let mut w = world(&["a", "b"]);
        if phase == "witness" {
            w.machines[0].bad = Some("witness");
        } else {
            w.factory.borrow_mut().push(format!("fail/{phase}"));
        }
        let error = w.crash(0, 5).unwrap_err();
        assert!(error.contains(phase), "{phase}: {error}");
        assert!(w.failure.is_some());
        let trace = w.factory.borrow().clone();
        let terminations = trace.iter().filter(|x| x.contains(":terminate:")).count();
        assert_eq!(
            terminations,
            if phase == "boot" { 2 } else { 3 },
            "{phase}: {trace:?}"
        );
        if phase == "boot" {
            assert!(trace.iter().any(|x| x == "partial-boot-cleanup"));
        }
        assert!(w.crash(0, 6).unwrap_err().contains("terminal"));
        assert!(w.lifecycle(0, faults::Lifecycle::Pause, 6).is_err());
        assert!(w.reboot(0, 6).is_err());
        assert!(w.deliver_due(0).is_err());
        assert!(WorldSnapshot::take(&mut w).is_err());
        assert!(simulate(&mut w, &Value::Null, 10, &None, None, &mut || {
            panic!("terminal world reached host boundary")
        })
        .is_err());
        assert_eq!(
            *w.factory.borrow(),
            trace,
            "terminal reuse performed activity"
        );
    }
}
#[test]
fn pause_resume_preserves_owed_io_and_pays_it_once_after_resume() {
    let mut w = world(&["a"]);
    w.machines[0].time = 5;
    w.machines[0].owed = Some(7);
    w.lifecycle(0, faults::Lifecycle::Pause, 5).unwrap();
    assert!(w.machines[0].paused);
    assert_eq!(w.machines[0].owed, Some(7));
    w.lifecycle(0, faults::Lifecycle::Resume, 20).unwrap();
    assert!(!w.machines[0].paused);
    assert_eq!(w.machines[0].time, 20);
    assert_eq!(w.machines[0].owed, Some(7));
    let request = SliceRequest {
        hard_limit: 23,
        early_slack: 0,
        observe_console: false,
    };
    w.machines[0]
        .advance(&mut (), request, &mut vec![])
        .unwrap();
    assert_eq!(w.machines[0].owed, Some(4));
    let request = SliceRequest {
        hard_limit: 27,
        early_slack: 0,
        observe_console: false,
    };
    w.machines[0]
        .advance(&mut (), request, &mut vec![])
        .unwrap();
    assert_eq!(w.machines[0].owed, None);
    assert_eq!(w.machines[0].time, 27);
}
#[test]
fn crash_discards_held_io_and_management_but_keeps_transit_frames() {
    let mut w = world(&["a", "b"]);
    w.machines[0].common.time = 5;
    w.machines[0].owed = Some(7);
    w.strict_events[0].insert((8, 1), b"old".to_vec());
    w.schedule(
        0,
        9,
        Payload::Frame {
            from: 1,
            bytes: vec![0; 14],
        },
    );
    w.schedule(
        0,
        11,
        Payload::Frame {
            from: 1,
            bytes: vec![0; 14],
        },
    );
    w.lifecycle(0, faults::Lifecycle::Crash, 6).unwrap();
    assert!(w.machines[0].down);
    assert_eq!(w.machines[0].incarnation, 1);
    assert!(w.strict_events[0].is_empty());
    assert_eq!(w.events[0].len(), 2);
    assert_eq!(w.incarnations[0][0]["discarded_pending_io"], true);
    w.lifecycle(0, faults::Lifecycle::Reboot, 10).unwrap();
    assert!(!w.machines[0].down);
    assert_eq!(w.events[0].len(), 1);
    assert_eq!(w.dropped, 1);
    assert_eq!(w.incarnations[0][0]["discarded_management_bytes"], 3);
}
#[test]
fn time_fork_settles_owed_io_before_parking_and_snapshotting() {
    let mut w = world(&["a"]);
    w.machines[0].owed = Some(20);
    assert_eq!(
        simulate(
            &mut w,
            &Value::Null,
            100,
            &None,
            Some(Some(10)),
            &mut || Ok(())
        )
        .unwrap(),
        "barrier"
    );
    assert_eq!(w.machines[0].time, 20);
    assert!(w.machines[0].parked);
    assert_eq!(&*w.factory.borrow(), &["a:advance:20", "a:park:20"]);
    let snapshot = WorldSnapshot::take(&mut w).unwrap();
    w.machines[0].common.time = 70;
    w.machines[0].common.parked = false;
    w.schedule(0, 90, Payload::Serial(vec![1]));
    w.seq = 42;
    w.slices = 40;
    w.delivered = 5;
    assert_eq!(snapshot.restore(&mut w).unwrap(), 1);
    assert_eq!(w.machines[0].time, 20);
    assert!(w.machines[0].parked);
    assert!(w.events[0].is_empty());
    assert_eq!(w.seq, 0);
    assert_eq!(w.delivered, 0);
    assert_eq!(w.slices, 1);
}
#[test]
fn target_exit_and_host_boundary_errors_stop_the_same_loop() {
    let mut w = world(&["a", "b"]);
    w.machines[0].steps.push_back(Step::new(3, Stop::Exited));
    assert_eq!(
        simulate(
            &mut w,
            &Value::Null,
            100,
            &Some("a".into()),
            None,
            &mut || Ok(())
        )
        .unwrap(),
        "end_machine_exited"
    );
    assert_eq!(w.machines[1].time, 0);
    let mut w = world(&["a"]);
    assert_eq!(
        simulate(&mut w, &Value::Null, 100, &None, None, &mut || Err(
            "cancelled".into()
        ))
        .unwrap_err(),
        "cancelled"
    );
    assert_eq!(w.slices, 0);
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
fn partition(handle: &str) -> seedbox_control::Action {
    seedbox_control::Action::Partition {
        handle: handle.into(),
        left: vec!["a".into()],
        right: vec!["b".into()],
    }
}
#[test]
fn admission_rejects_invalid_worlds_before_execution() {
    let mut w = world(&[]);
    assert_eq!(
        simulate(&mut w, &Value::Null, 20, &None, None, &mut || Ok(())).unwrap_err(),
        "world requires at least one machine"
    );
    let mut w = world(&["a", "b"]);
    w.boot.pop();
    assert!(w.validate().unwrap_err().contains("lengths"));
    assert!(w.factory.borrow().is_empty());
    let w = world(&["a", "a"]);
    assert!(w.validate().unwrap_err().contains("names"));
    let mut w = world(&["a", "b"]);
    w.macs[1] = w.macs[0];
    assert!(w.validate().unwrap_err().contains("MACs"));
    let mut w = world(&["a"]);
    w.early = w.lookahead;
    assert!(w.validate().unwrap_err().contains("slack"));
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
        let error = simulate(&mut w, &Value::Null, 100, &None, None, &mut || Ok(())).unwrap_err();
        assert!(
            error.contains("invalid stopped observation"),
            "{bad}: {error}"
        );
        assert_eq!(w.control.triggers.records[0].seen, 0, "{bad}");
        assert!(w.faults.readiness.is_empty(), "{bad}");
        assert!(w.control.pending.is_none(), "{bad}");
        assert!(w.events.iter().all(BTreeMap::is_empty), "{bad}");
        assert_eq!(w.delivered + w.dropped, 0, "{bad}");
    }
    let mut w = world(&["a"]);
    w.machines[0].common.fault_effects.push((0, 0, true));
    assert!(
        simulate(&mut w, &Value::Null, 10, &None, None, &mut || Ok(()))
            .unwrap_err()
            .contains("undrained effects")
    );
    assert!(w.factory.borrow().is_empty());
}
#[test]
fn owed_io_is_split_at_strict_boundary_before_time_fork() {
    let mut w = world(&["a"]);
    w.machines[0].owed = Some(20);
    w.strict_events[0].insert((7, 1), b"strict".to_vec());
    assert_eq!(
        simulate(
            &mut w,
            &Value::Null,
            100,
            &None,
            Some(Some(10)),
            &mut || Ok(())
        )
        .unwrap(),
        "barrier"
    );
    assert_eq!(
        &*w.factory.borrow(),
        &[
            "a:advance:7",
            "a:control/strict:7",
            "a:advance:20",
            "a:park:20"
        ]
    );
    assert!(w.machines[0].owed.is_none());
}
#[test]
fn control_then_trigger_then_fault_at_equal_time() {
    let mut w = world(&["a", "b"]);
    configure_control(&mut w);
    w.faults
        .append(seedbox_control::Plan {
            id: 0,
            at_ns: 50,
            deadline_ns: 1000,
            action: partition("scheduled"),
        })
        .unwrap();
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
        simulate(
            &mut w,
            &json!({"control":{"placement":{}}}),
            120,
            &None,
            None,
            &mut || Ok(())
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
}
#[test]
fn equal_time_control_allocates_fault_before_trigger() {
    use seedbox_control::Plan;
    let mut w = world(&["a", "b", "c"]);
    configure_control(&mut w);
    w.faults
        .append(Plan {
            id: 0,
            at_ns: 50,
            deadline_ns: 1000,
            action: partition("scheduled"),
        })
        .unwrap();
    w.control
        .triggers
        .arm("a".into(), b"X".to_vec(), 0, 200, partition("trigger"))
        .unwrap();
    w.control.triggers.committed("a", 0, b'X', 50, 0).unwrap();
    let request = json!({"version":1,"id":1,"op":"partition","left":[{"machine":"b"}],"right":[{"machine":"c"}]});
    w.control
        .request(0, 0, format!("SBX_RPC1 1 1 {request}").as_bytes(), 0)
        .unwrap();
    simulate(&mut w, &Value::Null, 120, &None, None, &mut || Ok(())).unwrap();
    assert_eq!(w.control.log[0]["fault_id"], 2);
    assert_eq!(w.control.triggers.records[0].fault_id, Some(3));
    assert_eq!(w.faults.log.len(), 3);
    assert!(w.faults.log.iter().all(|row| row["applied_at_ns"] == 50));
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
    assert_eq!(
        simulate(&mut w, &Value::Null, 150, &None, None, &mut || Ok(())).unwrap(),
        "fault_failed"
    );
    assert_eq!(w.control.log[0]["delivered_at_ns"], 100);
    assert!(!w.control.has_undelivered_reply());
    assert!(w
        .factory
        .borrow()
        .iter()
        .any(|line| line.starts_with("a:control/SBX_REPLY1 ")));
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
            simulate(&mut w, &Value::Null, 150, &None, Some(Some(4)), &mut || Ok(
                ()
            ))
            .unwrap(),
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
            simulate(&mut w, &Value::Null, 100, &None, None, &mut || Ok(())).unwrap();
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
fn boot_snapshot_resets_plan_but_keeps_readiness_and_rebuilds_crashed_machine() {
    let mut w = world(&["a", "b"]);
    configure_control(&mut w);
    w.faults.management_ready(0, 1, 50).unwrap();
    w.control
        .request(
            0,
            2,
            b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}",
            2,
        )
        .unwrap();
    for m in &mut w.machines {
        m.common.time = 3;
        m.park().unwrap();
    }
    let snapshot = WorldSnapshot::take(&mut w).unwrap();
    w.machines[0].common.parked = false;
    w.machines[0].owed = Some(4);
    w.lifecycle(0, faults::Lifecycle::Crash, 8).unwrap();
    assert_eq!(w.machines[0].incarnation, 1);
    snapshot.restore(&mut w).unwrap();
    assert_eq!(w.machines[0].incarnation, 0);
    assert!(w.machines[0].parked);
    assert!(w.machines[0].owed.is_none());
    assert_eq!(
        w.faults.readiness,
        json!([{"machine":"a","at_ns":1}])
            .as_array()
            .unwrap()
            .clone()
    );
    assert!(w.control.pending.is_none());
    assert!(w.control.controller.is_none());
    assert!(w.incarnations.iter().all(Vec::is_empty));
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

#[test]
fn original_coordinator_receipts() {
    use seedbox_control::{Action, Plan};
    for variant in 0..16u64 {
        let mut w = world(&["a", "b", "c"]);
        configure_control(&mut w);
        w.run_seed = variant + 1;
        w.early = variant % 4;
        w.control.latency = 60;
        w.machines[1].owed = Some(13 + variant);
        w.schedule(2, 19, Payload::Serial(b"input".to_vec()));
        w.schedule(2, 19, Payload::Serial(b"second".to_vec()));
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
            w.faults
                .append(Plan {
                    id: 0,
                    at_ns: at,
                    deadline_ns: at + 100,
                    action,
                })
                .unwrap();
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
        send.frames.push(frame);
        w.machines[0]
            .steps
            .extend([console, ready, request, send, Step::new(150, Stop::Exited)]);
        w.machines[2].steps.push_back(Step::new(12, Stop::Halted));
        w.machines[2].timer = 27;
        let outcome = simulate(
            &mut w,
            &Value::Null,
            160,
            &Some("a".into()),
            None,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(w.delivered + w.dropped, 1, "variant {variant}");
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
        println!(
            "world-receipt:{}",
            json!({"variant":variant,"outcome":outcome,"machines":machines,"trace":*w.factory.borrow(),"events":events,"strict":w.strict_events.iter().map(|e|e.iter().collect::<Vec<_>>()).collect::<Vec<_>>(),"faults":w.faults.log,"readiness":w.faults.readiness,"control":w.control.log,"triggers":w.control.triggers.evidence().unwrap(),"incarnations":w.incarnations,"network":w.network_snapshot(),"slices":w.slices,"seq":w.seq,"delivered":w.delivered,"dropped":w.dropped})
        );
    }
}

#[test]
fn runtime_checkpoint_precedes_exact_time_fault_and_preserves_active_plan() {
    let mut w = world(&["a", "b"]);
    w.faults
        .append(seedbox_control::Plan {
            id: 0,
            at_ns: 10,
            deadline_ns: 20,
            action: partition("checkpoint-cut"),
        })
        .unwrap();
    assert_eq!(
        simulate_to(&mut w, &Value::Null, 30, &None, None, Some(10), &mut || Ok(
            ()
        ))
        .unwrap(),
        "checkpoint"
    );
    assert_eq!(w.faults.log[0]["status"], "unrequested");
    assert!(!w.faults.blocked(0, 1));
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    for _ in 0..2 {
        saved.restore_checkpoint(&mut w).unwrap();
        assert_eq!(
            simulate_to(&mut w, &Value::Null, 30, &None, None, Some(11), &mut || Ok(
                ()
            ))
            .unwrap(),
            "checkpoint"
        );
        assert!(w.faults.blocked(0, 1));
        assert_eq!(w.faults.log.len(), 1);
    }
}

#[test]
fn runtime_checkpoint_drains_committed_messages_before_selection_and_snapshot() {
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
        simulate_to(&mut w, &Value::Null, 30, &None, None, Some(10), &mut || Ok(
            ()
        ))
        .unwrap(),
        "checkpoint"
    );
    assert!(w.machines[0].fault_effects.is_empty());
    assert!(w.machines[0].control_requests.is_empty());
    assert_eq!(w.faults.readiness, [json!({"machine":"a", "at_ns":9})]);
    let boundary = w.control.boundary();
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    saved.restore_checkpoint(&mut w).unwrap();
    assert!(w.control.pending.is_some());
    assert_eq!(w.control.boundary(), boundary);
}

#[test]
fn runtime_snapshot_preserves_lifecycle_transit_and_continuation_rng_positions() {
    let mut w = world(&["a", "b"]);
    w.machines[0].time = 4;
    w.machines[0].paused = true;
    w.machines[0].incarnation = 2;
    w.machines[0].booted_at = 3;
    w.incarnations[0].push(json!({"incarnation":1}));
    w.paused_ingress[0] = 7;
    w.run_seed = 42;
    w.branch_identity = Some("captured-parent".into());
    w.schedule(
        1,
        15,
        Payload::Frame {
            from: 0,
            bytes: vec![1, 2, 3],
        },
    );
    w.faults.links.stream_sample((0, 1));
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    let expected = w.faults.links.stream_sample((0, 1));
    w.machines[0].paused = false;
    w.incarnations[0].clear();
    w.events[1].clear();
    w.paused_ingress[0] = 0;
    w.run_seed = 99;
    saved.restore_checkpoint(&mut w).unwrap();
    assert!(w.machines[0].paused);
    assert_eq!((w.machines[0].incarnation, w.machines[0].booted_at), (2, 3));
    assert_eq!(w.incarnations[0], [json!({"incarnation":1})]);
    assert_eq!(w.paused_ingress[0], 7);
    assert_eq!(w.events[1].len(), 1);
    let names = vec!["a".into(), "b".into()];
    let report = reseed_checkpoint(&mut w, &names, 42, 42, "ignored-continuation-id").unwrap();
    assert!(report.iter().all(|r| r["reseed"] == "continuation"));
    assert_eq!(w.run_seed, 42);
    assert_eq!(w.branch_identity.as_deref(), Some("captured-parent"));
    assert_eq!(w.faults.links.stream_sample((0, 1)), expected);
    saved.restore_checkpoint(&mut w).unwrap();
    reseed_checkpoint(&mut w, &names, 42, 43, "new-branch").unwrap();
    assert_eq!(w.run_seed, 43);
    assert_eq!(w.branch_identity.as_deref(), Some("new-branch"));
    assert_ne!(w.faults.links.stream_sample((0, 1)), expected);
}
