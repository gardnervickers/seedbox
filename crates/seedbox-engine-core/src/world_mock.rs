//! A scripted `WorldMachine` for coordinator tests: the legacy loop's and the
//! windowed scheduler's. Shared, so both run the same scripts.
use super::world::*;
use crate::{control, links, network_counters, world::boot_faults};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

/// Every machine's trace, in the order it happened. `Arc` so a world can
/// run on a thread pool.
pub(crate) type Trace = Arc<Mutex<Vec<String>>>;

#[derive(Default)]
pub(crate) struct MockClock {
    pub(crate) pv_backoff: u64,
    pub(crate) watchdog_deadline: Option<u64>,
    pub(crate) restore_calls: usize,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Step {
    pub(crate) at: u64,
    pub(crate) stop: Stop,
    pub(crate) console: Option<u8>,
    pub(crate) output: Vec<u8>,
    pub(crate) observations: Vec<seedbox_engine_protocol::observation::Kind>,
    pub(crate) requests: Vec<Vec<u8>>,
    pub(crate) effects: Vec<(u32, bool)>,
    pub(crate) frames: Vec<Vec<u8>>,
    /// Committed property lines, as the shared decoder produces them.
    #[serde(default)]
    pub(crate) properties: Vec<Vec<u8>>,
    /// A disk request admitted at the step, by VIRTIO_BLK_T_* kind.
    #[serde(default)]
    pub(crate) block: Option<u32>,
}
impl Step {
    pub(crate) fn new(at: u64, stop: Stop) -> Self {
        Self {
            at,
            stop,
            console: None,
            output: Vec::new(),
            observations: Vec::new(),
            requests: vec![],
            effects: vec![],
            frames: vec![],
            properties: vec![],
            block: None,
        }
    }
}
pub(crate) struct Mock {
    pub(crate) common: MachineState,
    pub(crate) trace: Trace,
    pub(crate) steps: VecDeque<Step>,
    pub(crate) owed: Option<u64>,
    pub(crate) irq: bool,
    pub(crate) timer: u64,
    pub(crate) bad: Option<&'static str>,
    pub(crate) throttleable: bool,
    pub(crate) hold: Option<crate::virtio_block::Hold>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Saved {
    common: MachineState,
    steps: VecDeque<Step>,
    irq: bool,
    timer: u64,
    #[serde(default)]
    hold: Option<crate::virtio_block::Hold>,
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
    pub(crate) fn new(name: &str, trace: Trace) -> Self {
        Self {
            common: MachineState::new(name.into()),
            trace,
            steps: VecDeque::new(),
            owed: None,
            irq: false,
            timer: u64::MAX,
            bad: None,
            throttleable: true,
            hold: None,
        }
    }
    fn record(&self, kind: &str) {
        self.trace
            .lock()
            .unwrap()
            .push(format!("{}:{kind}:{}", self.name, self.time));
    }
}
impl WorldMachine for Mock {
    type Factory = Trace;
    type Boot = ();
    type Clock = MockClock;
    type Snapshot = Saved;
    fn boot(trace: &Trace, name: &str, _: &(), _: bool, _: [u8; 32]) -> Result<Self, String> {
        if trace.lock().unwrap().iter().any(|x| x == "fail/boot") {
            trace.lock().unwrap().push("partial-boot-cleanup".into());
            return Err("boot failed".into());
        }
        let mut machine = Self::new(name, trace.clone());
        if trace.lock().unwrap().iter().any(|x| x == "fail/timer") {
            machine.bad = Some("timer");
        }
        if trace.lock().unwrap().iter().any(|x| x == "fail/throttle") {
            machine.bad = Some("throttle");
        }
        if trace.lock().unwrap().iter().any(|x| x == "fail/install") {
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
    fn succeed_branch(
        &mut self,
        old: &mut Self,
        at: u64,
        seed: u64,
        identity: Option<&str>,
    ) -> Result<Option<u64>, String> {
        if let Some(identity) = identity {
            self.record(&format!("succeed-branch/{identity}"));
        }
        self.succeed(old, at, seed)
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
    fn checkpoint_boundary(_: &MockClock) -> Result<(), String> {
        Ok(())
    }
    fn restore_boundary(clock: &mut MockClock) -> Result<(), String> {
        clock.restore_calls += 1;
        clock.pv_backoff = 0;
        clock.watchdog_deadline = None;
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
    fn seed_progress_jitter(&mut self, _: u64, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn seed_progress_jitter_branch(
        &mut self,
        _: u64,
        _: Option<&str>,
        _: &str,
    ) -> Result<(), String> {
        Ok(())
    }
    fn supports_throttle(&self) -> bool {
        self.throttleable
    }
    fn throttle(&mut self, speed_hz: Option<u64>) -> Result<Option<u64>, String> {
        match speed_hz {
            Some(speed_hz) => self.record(&format!("throttle/{speed_hz}")),
            None => self.record("throttle/full"),
        }
        if self.bad == Some("throttle") {
            return Err("throttle ACK lost".into());
        }
        Ok(speed_hz.map(|hz| {
            seedbox_control::effective_throttle_speed(
                2_000_000_000,
                seedbox_control::throttle_permille(2_000_000_000, hz),
            )
        }))
    }
    fn has_disk(&self) -> bool {
        true
    }
    fn snapshot_bytes(_: &Self::Snapshot) -> u64 {
        100
    }
    fn hold_block(&mut self, hold: Option<crate::virtio_block::Hold>) -> Result<(), String> {
        if self.common.held.is_some() {
            return Err("block device already holds a request".into());
        }
        self.record(if hold.is_some() {
            "hold/on"
        } else {
            "hold/off"
        });
        self.hold = hold;
        Ok(())
    }
    fn disk_latency(
        &mut self,
        latency: Option<crate::virtio_block::InjectedLatency>,
    ) -> Result<Option<crate::virtio_block::LatencyCounts>, String> {
        self.record(if latency.is_some() {
            "disk_latency/on"
        } else {
            "disk_latency/off"
        });
        Ok(latency
            .is_none()
            .then_some(crate::virtio_block::LatencyCounts {
                delayed: 4,
                deepest: 2,
            }))
    }
    fn preempt(&mut self, setting: Option<(u32, u64)>) -> Result<(), String> {
        self.record(match setting {
            Some((ppm, _)) => {
                if ppm == 0 {
                    return Err("zero preempt rate".into());
                }
                "preempt/on"
            }
            None => "preempt/off",
        });
        Ok(())
    }
    fn disk_errors(
        &mut self,
        errors: Option<crate::virtio_block::InjectedErrors>,
    ) -> Result<Option<crate::virtio_block::ErrorCounts>, String> {
        self.record(if errors.is_some() {
            "disk_errors/on"
        } else {
            "disk_errors/off"
        });
        Ok(errors
            .is_none()
            .then_some(crate::virtio_block::ErrorCounts {
                matched: 3,
                failed: 2,
            }))
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
            hold: self.hold,
        })
    }
    fn restore(&mut self, s: &Saved) -> Result<u64, String> {
        if self.bad == Some("restore") {
            self.time = 999;
            return Err("partial restore failed".into());
        }
        self.common = s.common.clone();
        self.steps = s.steps.clone();
        self.owed = None;
        self.irq = s.irq;
        self.timer = s.timer;
        self.hold = s.hold;
        self.common.clear_effects();
        Ok(1)
    }
    fn advance(
        &mut self,
        _: &mut MockClock,
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
        for kind in step.observations {
            self.common.observations.push(crate::observation::Entry {
                time_ns: step.at,
                incarnation: self.common.incarnation(),
                kind,
            });
        }
        if request.observe_console && !step.output.is_empty() {
            self.common.committed_output.push((step.at, step.output));
        }
        self.common.committed_console = step
            .console
            .filter(|_| request.observe_console)
            .map(|byte| (step.at, byte));
        self.common
            .control_requests
            .extend(step.requests.into_iter().map(|bytes| (step.at, bytes)));
        let incarnation = self.common.incarnation();
        self.common.committed_properties.extend(
            step.properties
                .into_iter()
                .map(|line| (step.at, incarnation, Some(line))),
        );
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
        // A held request stops the machine at the step, before its effect.
        let matched = step
            .block
            .zip(self.hold.as_mut())
            .is_some_and(|(kind, hold)| {
                let matches = match hold.op {
                    seedbox_control::DiskOp::Any => true,
                    seedbox_control::DiskOp::Read => kind == 0,
                    seedbox_control::DiskOp::Write => kind == 1,
                    seedbox_control::DiskOp::Flush => kind == 4,
                };
                hold.seen += u64::from(matches);
                matches && hold.seen == hold.nth
            });
        if matched {
            let trigger = self.hold.take().map(|h| h.trigger_id).unwrap_or(0);
            self.record("held");
            return Ok(Stop::Held(trigger));
        }
        match step.stop {
            Stop::Exited => self.common.state = State::Exited(0),
            Stop::Halted => self.common.state = State::Halted,
            Stop::Barrier => self.common.commit_park(),
            _ => (),
        }
        self.record("advance");
        Ok(step.stop)
    }
}
pub(crate) fn world(names: &[&str]) -> World<Mock> {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let trace = Trace::default();
    let n = names.len();
    World {
        scheduler_continuation: None,
        input_program: Default::default(),
        observations: None,
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
        clock: MockClock::default(),
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
        boundaries: 0,
        properties: Default::default(),
        choices: Default::default(),
        anchor_captures: Default::default(),
        captured_anchor: None,
        resume_after_anchor: false,
    }
}
