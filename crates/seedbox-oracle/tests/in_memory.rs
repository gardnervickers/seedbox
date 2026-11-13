//! A fake replicated log run through every trait, as a runner would drive it,
//! and a fault profile run against a world that models every fault in force.
use seedbox_oracle::{nemesis::profile::Domain, *};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

const NODES: [&str; 3] = ["n1", "n2", "n3"];

/// Three replicas of one log. Appends reach every replica not cut off; a
/// heal repairs the cut-off replicas from n1 unless `lossy`.
#[derive(Default)]
struct Cluster {
    logs: [Vec<u64>; 3],
    cut: BTreeSet<usize>,
    lossy: bool,
}

type Shared = Arc<Mutex<Cluster>>;

struct Recorder(Mutex<Vec<Record<u64, usize>>>);

impl History<u64, usize> for Recorder {
    fn invoke(&self, process: u64, op: u64) -> u64 {
        let mut records = self.0.lock().unwrap();
        let index = records.len() as u64;
        records.push(Record {
            index,
            process,
            time_ns: index * 10,
            event: Event::Invoke { op },
        });
        index
    }
    fn complete(&self, invoke: u64, completion: Completion<usize>) {
        let mut records = self.0.lock().unwrap();
        let index = records.len() as u64;
        let process = records[invoke as usize].process;
        let event = match completion {
            Completion::Ok(value) => Event::Ok { invoke, value },
            Completion::Fail(error) => Event::Fail { invoke, error },
            Completion::Info(error) => Event::Info { invoke, error },
        };
        records.push(Record {
            index,
            process,
            time_ns: index * 10,
            event,
        });
    }
    fn records(&self) -> Vec<Record<u64, usize>> {
        self.0.lock().unwrap().clone()
    }
}

struct Counter(u64);

impl Random for Counter {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }
}

/// Appends unique values `process * 1000 + counter`.
struct Appender {
    cluster: Shared,
    process: u64,
    counter: u64,
}

impl Client for Appender {
    type Op = u64;
    type Value = usize;
    fn generate(&mut self, _: &mut dyn Random) -> Option<u64> {
        self.counter += 1;
        Some(self.process * 1000 + self.counter)
    }
    fn invoke(&mut self, op: &u64) -> Completion<usize> {
        let mut cluster = self.cluster.lock().unwrap();
        let cut = cluster.cut.clone();
        for (i, log) in cluster.logs.iter_mut().enumerate() {
            if !cut.contains(&i) {
                log.push(*op);
            }
        }
        Completion::Ok(cluster.logs[0].len())
    }
}

struct FakeWorld {
    cluster: Shared,
    now_ns: u64,
    applied: Vec<Action>,
}

impl World for FakeWorld {
    fn now_ns(&mut self) -> Result<u64, Error> {
        Ok(self.now_ns)
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        self.now_ns += duration.as_nanos() as u64;
        Ok(())
    }
    fn apply(&mut self, action: &Action) -> Result<(), Error> {
        self.applied.push(action.clone());
        let mut cluster = self.cluster.lock().unwrap();
        match action {
            Action::Partition { left, .. } => {
                for node in left {
                    cluster
                        .cut
                        .insert(NODES.iter().position(|n| n == node).unwrap());
                }
            }
            Action::Heal { .. } => {
                let healed = std::mem::take(&mut cluster.cut);
                if !cluster.lossy {
                    let full = cluster.logs[0].clone();
                    for i in healed {
                        cluster.logs[i] = full.clone();
                    }
                }
            }
            other => return Err(format!("unsupported {}", other.name()).into()),
        }
        Ok(())
    }
}

/// Cuts one random replica off from the rest; leaves it cut for `heal`.
#[derive(Default)]
struct Isolate {
    open: Vec<String>,
}

impl Nemesis for Isolate {
    fn run(&mut self, world: &mut dyn World, random: &mut dyn Random) -> Result<(), Error> {
        let node = NODES[1 + (random.next_u64() % 2) as usize];
        let rest = NODES.iter().filter(|n| **n != node).map(|n| n.to_string());
        let handle = format!("isolate-{node}");
        world.apply(&Action::Partition {
            handle: handle.clone(),
            left: vec![node.into()],
            right: rest.collect(),
        })?;
        self.open.push(handle);
        world.sleep(Duration::from_secs(1))
    }
    fn heal(&mut self, world: &mut dyn World) -> Result<(), Error> {
        for handle in std::mem::take(&mut self.open) {
            world.apply(&Action::Heal { handle })?;
        }
        Ok(())
    }
}

struct Reads(Shared);

impl FinalRead for Reads {
    type Read = Vec<u64>;
    fn nodes(&self) -> Vec<String> {
        NODES.iter().map(|n| n.to_string()).collect()
    }
    fn ready(&mut self) -> bool {
        true
    }
    fn exited(&mut self, _: &str) -> bool {
        false
    }
    fn view(&mut self, node: &str) -> Value {
        json!(self.read(node).map(|log| log.len()).ok())
    }
    fn read(&mut self, node: &str) -> Result<Vec<u64>, String> {
        let index = NODES
            .iter()
            .position(|n| *n == node)
            .ok_or("no such node")?;
        Ok(self.0.lock().unwrap().logs[index].clone())
    }
}

/// Every acknowledged append is on every replica.
struct Survives;

impl Checker<u64, usize, Vec<u64>> for Survives {
    fn name(&self) -> &str {
        "acknowledged appends survive on every replica"
    }
    fn check(&self, history: &[Record<u64, usize>], reads: &[Read<Vec<u64>>]) -> Verdict {
        let acked: BTreeSet<u64> = history
            .iter()
            .filter_map(|r| match r.event {
                Event::Ok { invoke, .. } => match history[invoke as usize].event {
                    Event::Invoke { op } => Some(op),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let mut missing = Vec::new();
        for read in reads {
            let Ok(log) = &read.result else {
                return Verdict {
                    validity: Validity::Unknown,
                    summary: format!("{} unread", read.node),
                    details: Value::Null,
                };
            };
            let held: BTreeSet<u64> = log.iter().copied().collect();
            let lost = acked.difference(&held).count();
            if lost > 0 {
                missing.push(json!({"node": read.node, "missing": lost}));
            }
        }
        Verdict {
            validity: if missing.is_empty() {
                Validity::Valid
            } else {
                Validity::Invalid
            },
            summary: format!("{} acked appends checked", acked.len()),
            details: json!({ "acked": acked.len(), "missing": missing }),
        }
    }
}

struct Toy {
    cluster: Shared,
    history: Arc<Recorder>,
}

impl Scenario for Toy {
    type Op = u64;
    type Value = usize;
    type Read = Vec<u64>;
    type Client = Appender;
    fn name(&self) -> &str {
        "toy-log"
    }
    fn setup(&mut self, _: &mut dyn World) -> Result<(), Error> {
        Ok(())
    }
    fn history(&self) -> Arc<dyn History<u64, usize>> {
        self.history.clone()
    }
    fn clients(&mut self) -> Vec<Appender> {
        (0..2)
            .map(|process| Appender {
                cluster: self.cluster.clone(),
                process,
                counter: 0,
            })
            .collect()
    }
    fn nemesis(&mut self) -> Box<dyn Nemesis> {
        Box::new(Isolate::default())
    }
    fn final_read(&mut self) -> Box<dyn FinalRead<Read = Vec<u64>>> {
        Box::new(Reads(self.cluster.clone()))
    }
    fn checkers(&self) -> Checkers<u64, usize, Vec<u64>> {
        vec![Box::new(Survives)]
    }
}

/// Drive the phases in order on one thread: three rounds of operations
/// before, during and after the nemesis, then heal, read and check.
fn run<S: Scenario>(scenario: &mut S, world: &mut dyn World) -> Report {
    let mut random = Counter(7);
    scenario.setup(world).unwrap();
    let history = scenario.history();
    let mut clients = scenario.clients();
    let mut round = |random: &mut Counter| {
        for (process, client) in clients.iter_mut().enumerate() {
            let op = client.generate(random).unwrap();
            let invoke = history.invoke(process as u64, op.clone());
            history.complete(invoke, client.invoke(&op));
        }
    };
    round(&mut random);
    let mut nemesis = scenario.nemesis();
    nemesis.run(world, &mut random).unwrap();
    round(&mut random);
    nemesis.heal(world).unwrap();
    round(&mut random);
    let mut reader = scenario.final_read();
    assert!(reader.ready());
    let views: Vec<Value> = reader.nodes().iter().map(|n| reader.view(n)).collect();
    assert!(!views.is_empty());
    let reads: Vec<Read<S::Read>> = reader
        .nodes()
        .into_iter()
        .map(|node| Read {
            result: reader.read(&node),
            node,
        })
        .collect();
    let records = history.records();
    Report {
        verdicts: scenario
            .checkers()
            .iter()
            .map(|c| (c.name().to_string(), c.check(&records, &reads)))
            .collect(),
        ..Report::default()
    }
}

fn outcome(lossy: bool) -> (Report, Vec<Action>, Vec<Record<u64, usize>>) {
    let cluster: Shared = Arc::new(Mutex::new(Cluster {
        lossy,
        ..Default::default()
    }));
    let history = Arc::new(Recorder(Mutex::new(Vec::new())));
    let mut scenario = Toy {
        cluster: cluster.clone(),
        history: history.clone(),
    };
    let mut world = FakeWorld {
        cluster,
        now_ns: 0,
        applied: Vec::new(),
    };
    let report = run(&mut scenario, &mut world);
    (report, world.applied, history.records())
}

#[test]
fn a_repaired_partition_keeps_every_acknowledged_append() {
    let (report, applied, records) = outcome(false);
    assert_eq!(report.exit_code(), Report::OK);
    let (name, verdict) = &report.verdicts[0];
    assert_eq!(name, "acknowledged appends survive on every replica");
    assert_eq!(verdict.condition(), Some(true));
    assert_eq!(verdict.details["acked"], 6);
    let [Action::Partition { handle, left, .. }, Action::Heal { handle: healed }] = &applied[..]
    else {
        panic!("{applied:?}");
    };
    assert_eq!((left.len(), handle), (1, healed));
    assert_eq!(records.len(), 12);
    assert_eq!(
        serde_json::to_value(&records[1]).unwrap(),
        json!({"index": 1, "process": 0, "time_ns": 10, "type": "ok", "invoke": 0, "value": 1})
    );
}

#[test]
fn appends_lost_by_a_partition_are_a_violation() {
    let (report, _, _) = outcome(true);
    assert_eq!(report.exit_code(), Report::INVALID);
    let verdict = &report.verdicts[0].1;
    assert_eq!(verdict.condition(), Some(false));
    assert_eq!(verdict.details["missing"][0]["missing"], 2);
    let unknown = Verdict {
        validity: Validity::Unknown,
        summary: "n2 unread".into(),
        details: Value::Null,
    };
    assert_eq!(unknown.condition(), None);
    let undecided = Report {
        verdicts: vec![("check".into(), unknown)],
        ..Report::default()
    };
    assert_eq!(undecided.exit_code(), Report::UNKNOWN);
}

/// A violated check fails the run whatever else happened; otherwise a failed
/// step, an undecided check or no check at all leaves it incomplete.
#[test]
fn exit_codes_rank_violations_then_failed_steps_and_undecided_checks() {
    let verdict = |validity| Verdict {
        validity,
        summary: String::new(),
        details: Value::Null,
    };
    let report = |validities: &[Validity], failed: bool| Report {
        verdicts: validities
            .iter()
            .map(|v| ("check".to_string(), verdict(*v)))
            .collect(),
        run_error: failed.then(|| Error("shape: invalid_request".into())),
        ..Report::default()
    };
    use Validity::*;
    for (validities, failed, code) in [
        (&[Valid][..], false, Report::OK),
        (&[Valid], true, Report::UNKNOWN),
        (&[Valid, Unknown], false, Report::UNKNOWN),
        (&[], false, Report::UNKNOWN),
        (&[], true, Report::UNKNOWN),
        (&[Valid, Invalid], false, Report::INVALID),
        (&[Valid, Invalid], true, Report::INVALID),
        (&[Unknown, Invalid], true, Report::INVALID),
    ] {
        let report = report(validities, failed);
        assert_eq!(report.exit_code(), code, "{report:?}");
    }
    let mut failed = report(&[Valid], false);
    failed.heal_error = Some(Error("heal p1: no such handle".into()));
    assert_eq!(failed.exit_code(), Report::UNKNOWN);
    failed.heal_error = None;
    failed.panics.push(ClientPanic {
        client: 0,
        message: "boom".into(),
    });
    assert_eq!(failed.exit_code(), Report::UNKNOWN);
    assert_eq!(failed.failures(), ["client 0 panicked: boom"]);
    for step in [
        |r: &mut Report| r.restart_errors.push(Error("restart n1: refused".into())),
        |r: &mut Report| r.break_error = Some(Error("handler failed".into())),
        |r: &mut Report| r.flush_error = Some(Error("no flush reply".into())),
    ] {
        let mut failed = report(&[Valid], false);
        step(&mut failed);
        assert_eq!(failed.exit_code(), Report::UNKNOWN, "{failed:?}");
        assert_eq!(failed.failures().len(), 1, "{failed:?}");
    }
}

/// Every kind the profile has, overlapping: faults hold for up to three
/// intervals.
const PROFILE: &str = r#"
start_ns: 1000000000
end_ns: 30000000000
interval_ns: 1000000000
max_faults: 20
targets: [n1, n2, n3]
faults:
  - {kind: partition, weight: 2, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: partition, duration_ns: {min: 500000000, max: 3000000000}, targets: [n1, n2], from: [n3]}
  - {kind: shape, duration_ns: {min: 500000000, max: 3000000000},
     latency_ns: {min: 1000000, max: 50000000}, jitter_ns: 500000, to: [n1, n2, n3, client]}
  - {kind: shape, duration_ns: {min: 500000000, max: 3000000000},
     latency_ns: {min: 1000000, max: 50000000}, targets: [n1, n2], to: [n3]}
  - {kind: crash, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: pause, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: pause_process, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: stop, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: clock_skew, duration_ns: {min: 500000000, max: 3000000000}, max_offset_ns: 5000000000}
  - {kind: clock_drift, duration_ns: {min: 500000000, max: 3000000000}, max_rate_ppb: 1000000}
  - {kind: throttle, duration_ns: {min: 500000000, max: 3000000000}, factor_permille: 4000}
  - {kind: disk_error, duration_ns: {min: 500000000, max: 3000000000}, request: write, ppm: 1000,
     targets: [n1, n2]}
  - {kind: disk_latency, duration_ns: {min: 500000000, max: 3000000000}, request: flush,
     latency_ns: {min: 1000000, max: 20000000}, targets: [n1, n2]}
  - {kind: preempt, duration_ns: {min: 500000000, max: 3000000000}, ppm: 10000}
"#;

/// A world that models every fault in force and refuses what the engine
/// would: a second lifecycle fault on a machine, an undo of nothing, a heal
/// of a handle not in force, an unknown machine. `refuse` refuses the
/// numbered apply it picks, as the world refuses an action it cannot take.
struct Modelled {
    now_ns: u64,
    applied: Vec<(u64, Action)>,
    attempts: usize,
    handles: BTreeSet<String>,
    /// Machines with a lifecycle fault in force, by the action that undoes it.
    down: std::collections::BTreeMap<String, &'static str>,
    refuse: Refusal,
}

/// Whether the world refuses an apply, by its number and action.
type Refusal = Box<dyn FnMut(usize, &Action) -> bool>;

impl Modelled {
    fn new(refuse: impl FnMut(usize, &Action) -> bool + 'static) -> Self {
        Self {
            now_ns: 5_000_000_000,
            applied: Vec::new(),
            attempts: 0,
            handles: BTreeSet::new(),
            down: Default::default(),
            refuse: Box::new(refuse),
        }
    }

    fn in_force(&self) -> usize {
        self.handles.len() + self.down.len()
    }
}

impl World for Modelled {
    fn now_ns(&mut self) -> Result<u64, Error> {
        Ok(self.now_ns)
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        assert!(!duration.is_zero());
        self.now_ns += duration.as_nanos() as u64;
        Ok(())
    }
    fn apply(&mut self, action: &Action) -> Result<(), Error> {
        self.attempts += 1;
        if (self.refuse)(self.attempts, action) {
            return Err(format!("{}: invalid_state: refused", action.name()).into());
        }
        let known = |machine: &String| ["n1", "n2", "n3", "client"].contains(&machine.as_str());
        let undo = match action {
            Action::Stop { .. } => Some("restart"),
            Action::PauseProcess { .. } => Some("resume_process"),
            Action::Pause { .. } => Some("resume"),
            Action::Crash { .. } => Some("reboot"),
            _ => None,
        };
        match action {
            Action::Restart { machine }
            | Action::ResumeProcess { machine }
            | Action::Resume { machine }
            | Action::Reboot { machine } => {
                assert_eq!(self.down.remove(machine), Some(action.name()), "{action:?}");
            }
            Action::Heal { handle } => assert!(self.handles.remove(handle), "{action:?}"),
            Action::Partition {
                handle,
                left,
                right,
            } => {
                assert!(left.iter().chain(right).all(known) && !right.contains(&left[0]));
                assert!(self.handles.insert(handle.clone()));
            }
            Action::Shape {
                handle, from, to, ..
            } => {
                assert!(from.iter().chain(to).all(known) && !to.contains(&from[0]));
                assert!(self.handles.insert(handle.clone()));
            }
            Action::ClockSkew {
                handle,
                offset_ns: Some(_),
                max_offset_ns: None,
                ..
            }
            | Action::ClockDrift {
                handle,
                rate_ppb: Some(_),
                max_rate_ppb: None,
                ..
            }
            | Action::Throttle { handle, .. }
            | Action::Preempt { handle, .. } => {
                assert!(self.handles.insert(handle.clone()));
            }
            Action::DiskError {
                handle, machine, ..
            }
            | Action::DiskLatency {
                handle, machine, ..
            } => {
                assert!(["n1", "n2"].contains(&machine.as_str()), "{action:?}");
                assert!(self.handles.insert(handle.clone()));
            }
            other => {
                let machine = other.machine().expect("a lifecycle action names a machine");
                assert!(known(&machine.to_owned()));
                let undo = undo.unwrap_or_else(|| panic!("unexpected {other:?}"));
                assert!(
                    self.down.insert(machine.into(), undo).is_none(),
                    "{other:?} on a machine already down"
                );
            }
        }
        self.applied.push((self.now_ns, action.clone()));
        Ok(())
    }
}

fn profile() -> nemesis::Profile {
    serde_yaml_ng::from_str(PROFILE).unwrap()
}

/// Run the profile to completion with `seed` and return what it applied.
fn applied(seed: u64) -> Vec<(u64, Action)> {
    let mut world = Modelled::new(|_, _| false);
    let mut profile = profile();
    profile.run(&mut world, &mut Rng::new(seed)).unwrap();
    assert_eq!(world.in_force(), 0, "every fault ends within run");
    profile.heal(&mut world).unwrap();
    assert!(world.now_ns >= 5_000_000_000 + 30_000_000_000);
    world.applied
}

#[test]
fn a_profile_applies_the_same_sequence_for_the_same_seed() {
    let first = applied(7);
    assert_eq!(first, applied(7));
    assert_ne!(first, applied(8));
    // Faults start inside the window, at most max_faults of them, and every
    // kind the profile names comes up.
    let starts: Vec<_> = first
        .iter()
        .filter(|(_, action)| {
            !matches!(
                action,
                Action::Heal { .. }
                    | Action::Restart { .. }
                    | Action::ResumeProcess { .. }
                    | Action::Resume { .. }
                    | Action::Reboot { .. }
            )
        })
        .collect();
    assert!(!starts.is_empty() && starts.len() <= 20, "{}", starts.len());
    assert_eq!(first.len(), 2 * starts.len());
    assert!(starts
        .iter()
        .all(|(at, _)| (6_000_000_000..35_000_000_000).contains(at)));
    let all: Vec<_> = (0..20)
        .flat_map(applied)
        .map(|(_, action)| action)
        .collect();
    let kinds: BTreeSet<_> = all.iter().map(Action::name).collect();
    assert_eq!(kinds.len(), 17, "{kinds:?}");
    // The narrowed partition and shape cut or shape n1 or n2 to n3 alone;
    // the others face their side without the target. Both kinds of each
    // come up.
    let faces = |near: &String, far: &[String], side: &[&str]| {
        let rest: Vec<_> = side.iter().filter(|m| **m != near).collect();
        (far == ["n3"] && ["n1", "n2"].contains(&near.as_str())) || far.iter().eq(rest)
    };
    let mut sides = BTreeSet::new();
    for action in &all {
        let (near, far, side) = match action {
            Action::Partition { left, right, .. } => (&left[0], right, &["n1", "n2", "n3"][..]),
            Action::Shape { from, to, .. } => (&from[0], to, &["n1", "n2", "n3", "client"][..]),
            _ => continue,
        };
        assert!(faces(near, far, side), "{action:?}");
        sides.insert((action.name(), far.len()));
    }
    assert_eq!(
        sides,
        BTreeSet::from([
            ("partition", 1),
            ("partition", 2),
            ("shape", 1),
            ("shape", 3)
        ])
    );
}

#[test]
fn an_invalid_profile_is_refused_before_anything_applies() {
    let mut profile = profile();
    profile.faults.push(
        serde_json::from_value(json!({"kind": "throttle", "factor_permille": 10,
                                      "duration_ns": {"min": 1, "max": 2}}))
        .unwrap(),
    );
    let mut world = Modelled::new(|_, _| false);
    let error = profile.run(&mut world, &mut Rng::new(7)).unwrap_err();
    let own = control::validate_throttle(10).unwrap_err();
    assert_eq!(error.0, format!("fault profile throttle: {own}"));
    assert!(world.applied.is_empty());
}

#[test]
fn heal_ends_every_fault_still_in_force_after_a_refused_apply() {
    let mut world = Modelled::new(|attempt, _| attempt == 9);
    let mut profile = profile();
    let error = profile.run(&mut world, &mut Rng::new(7)).unwrap_err();
    assert!(error.0.ends_with("invalid_state: refused"), "{error}");
    assert!(world.in_force() >= 2, "faults overlap before the refusal");
    profile.heal(&mut world).unwrap();
    assert_eq!(world.in_force(), 0);
    // Nothing is left to heal.
    let applied = world.applied.len();
    profile.heal(&mut world).unwrap();
    assert_eq!(world.applied.len(), applied);
}

#[test]
fn heal_tries_every_fault_and_reports_the_first_failure() {
    let mut world = Modelled::new(|attempt, _| attempt == 9);
    let mut profile = profile();
    profile.run(&mut world, &mut Rng::new(7)).unwrap_err();
    let before = world.in_force();
    // Refuse the first undo heal tries; the rest still apply.
    let first_undo = world.attempts + 1;
    world.refuse = Box::new(move |attempt, _| attempt == first_undo);
    let error = profile.heal(&mut world).unwrap_err();
    assert!(error.0.contains("invalid_state: refused"), "{error}");
    assert_eq!(world.in_force(), 1, "{before} in force, one undo refused");
    // The refused fault is still in force, and the next heal ends it.
    profile.heal(&mut world).unwrap();
    assert_eq!(world.in_force(), 0);
}

/// A world whose engine lacks the guest yield hook: it refuses every
/// preempt as unsupported, as x86_64 PV does, and counts the attempts.
struct NoYield {
    world: Modelled,
    preempts: usize,
}

impl NoYield {
    fn new(refuse: impl FnMut(usize, &Action) -> bool + 'static) -> Self {
        Self {
            world: Modelled::new(refuse),
            preempts: 0,
        }
    }
}

impl World for NoYield {
    fn now_ns(&mut self) -> Result<u64, Error> {
        self.world.now_ns()
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        self.world.sleep(duration)
    }
    fn apply(&mut self, action: &Action) -> Result<(), Error> {
        match action {
            Action::Preempt { .. } => {
                self.preempts += 1;
                Err("preempt: unsupported: no guest yield hook".into())
            }
            _ => self.world.apply(action),
        }
    }
}

#[test]
fn a_profile_can_skip_the_kinds_its_world_does_not_support() {
    let mut refused = 0;
    for seed in 0..20 {
        // Without the option, an unsupported kind fails the run.
        let mut world = NoYield::new(|_, _| false);
        let mut strict = profile();
        let failed = strict.run(&mut world, &mut Rng::new(seed));
        let before = world.world.applied.clone();
        strict.heal(&mut world).unwrap();
        // With it, the same seed applies the same faults up to that start,
        // skips it, and never draws the kind again.
        let mut world = NoYield::new(|_, _| false);
        let mut skipping = profile();
        skipping.skip_unsupported = true;
        skipping.run(&mut world, &mut Rng::new(seed)).unwrap();
        assert_eq!(world.world.in_force(), 0);
        let applied = &world.world.applied;
        let skipped: Vec<_> = skipping.unsupported().collect();
        match failed {
            Ok(()) => {
                assert_eq!(*applied, before);
                assert_eq!((world.preempts, skipped), (0, vec![]));
            }
            Err(error) => {
                assert_eq!(error.0, "preempt: unsupported: no guest yield hook");
                assert_eq!(applied[..before.len()], before);
                assert_eq!((world.preempts, skipped), (1, vec!["preempt"]));
                refused += 1;
            }
        }
    }
    assert!(refused > 0, "some seed draws preempt");
}

#[test]
fn a_kind_refused_as_unsupported_is_dropped_with_every_entry() {
    let mut refused = 0;
    for seed in 0..10 {
        let mut twice = profile();
        twice.skip_unsupported = true;
        // A second preempt entry, drawn more often than every other kind.
        twice.faults.push(
            serde_json::from_value(json!({"kind": "preempt", "weight": 20, "ppm": 50_000,
                "duration_ns": {"min": 500_000_000, "max": 1_000_000_000}}))
            .unwrap(),
        );
        let mut world = NoYield::new(|_, _| false);
        twice.run(&mut world, &mut Rng::new(seed)).unwrap();
        assert!(
            world.preempts <= 1,
            "seed {seed}: {} preempts",
            world.preempts
        );
        refused += world.preempts;
    }
    assert!(refused > 0, "some seed draws preempt");
}

#[test]
fn skipping_unsupported_kinds_still_fails_on_other_refusals_and_when_none_is_left() {
    let mut skipping = profile();
    skipping.skip_unsupported = true;
    let mut world = NoYield::new(|attempt, _| attempt == 9);
    let error = skipping.run(&mut world, &mut Rng::new(7)).unwrap_err();
    assert!(error.0.ends_with("invalid_state: refused"), "{error}");
    skipping.heal(&mut world).unwrap();
    assert_eq!(world.world.in_force(), 0);

    let mut only = profile();
    only.skip_unsupported = true;
    only.faults.retain(|fault| fault.name() == "preempt");
    let mut world = NoYield::new(|_, _| false);
    let error = only.run(&mut world, &mut Rng::new(7)).unwrap_err();
    assert_eq!(
        error.0,
        "fault profile has no supported fault kind left: preempt: unsupported: no guest yield hook"
    );
    assert_eq!(world.preempts, 1);
    assert!(world.world.applied.is_empty());
}

/// One target, a crash and a disk latency: slowdisk's shape.
const SINGLE: &str = r#"
start_ns: 1000000000
end_ns: 18000000000
interval_ns: 1000000000
targets: [n1]
faults:
  - {kind: crash, downtime_ns: {min: 900000000, max: 1100000000}}
  - {kind: disk_latency, weight: 2, request: any, duration_ns: {min: 1000000000, max: 3000000000},
     latency_ns: {min: 25000000, max: 40000000}}
"#;

/// `PROFILE` as it was on origin/main 0875cc887, frozen: the digest below is
/// of this text, so `PROFILE` can gain entries without moving it.
const PINNED: &str = r#"
start_ns: 1000000000
end_ns: 30000000000
interval_ns: 1000000000
max_faults: 20
targets: [n1, n2, n3]
faults:
  - {kind: partition, weight: 2, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: shape, duration_ns: {min: 500000000, max: 3000000000},
     latency_ns: {min: 1000000, max: 50000000}, jitter_ns: 500000, to: [n1, n2, n3, client]}
  - {kind: crash, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: pause, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: pause_process, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: stop, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: clock_skew, duration_ns: {min: 500000000, max: 3000000000}, max_offset_ns: 5000000000}
  - {kind: clock_drift, duration_ns: {min: 500000000, max: 3000000000}, max_rate_ppb: 1000000}
  - {kind: throttle, duration_ns: {min: 500000000, max: 3000000000}, factor_permille: 4000}
  - {kind: disk_error, duration_ns: {min: 500000000, max: 3000000000}, request: write, ppm: 1000,
     targets: [n1, n2]}
  - {kind: disk_latency, duration_ns: {min: 500000000, max: 3000000000}, request: flush,
     latency_ns: {min: 1000000, max: 20000000}, targets: [n1, n2]}
  - {kind: preempt, duration_ns: {min: 500000000, max: 3000000000}, ppm: 10000}
"#;

/// FNV-1a over every applied (time, action) of seeds 0..32.
fn fingerprint(yaml: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for seed in 0..32 {
        let mut world = Modelled::new(|_, _| false);
        let mut profile: nemesis::Profile = serde_yaml_ng::from_str(yaml).unwrap();
        profile.run(&mut world, &mut Rng::new(seed)).unwrap();
        for (at, action) in &world.applied {
            for byte in serde_json::to_vec(&(at, action)).unwrap() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    hash
}

fn with_overlap(yaml: &str, overlap: &str) -> String {
    format!("{yaml}overlap: {overlap}\n")
}

#[test]
fn the_default_overlap_applies_the_same_sequence_as_before_it_existed() {
    // Digests of seeds 0..32 taken on origin/main 0875cc887, before
    // profiles had `overlap`. They hash the serialized actions, so a change
    // to `Action`'s wire format changes them with no profile change. Re-pin
    // only after confirming the new digests on the previous commit with the
    // field omitted.
    let before = [
        (PINNED, 0x52af_f1af_96f7_7bfe),
        (SINGLE, 0x51c8_b1d5_745c_d916),
    ];
    for (yaml, digest) in before {
        assert_eq!(fingerprint(yaml), digest);
        assert_eq!(fingerprint(&with_overlap(yaml, "per_target")), digest);
    }
    assert_ne!(
        fingerprint(&with_overlap(SINGLE, "per_domain")),
        before[1].1
    );
}

/// A start or an end of a fault, with the kinds of the other faults holding
/// its target at that moment.
#[derive(Debug)]
struct Step {
    starts: bool,
    kind: &'static str,
    others: Vec<&'static str>,
}

/// Walk an applied sequence, tracking which faults hold which target.
fn events(applied: &[(u64, Action)]) -> Vec<Step> {
    use std::collections::BTreeMap;
    // A key per fault in force: its handle, or the machine for lifecycle.
    let mut held: BTreeMap<String, (&'static str, String)> = BTreeMap::new();
    let mut events = Vec::new();
    for (_, action) in applied {
        let lifecycle = |machine: &str| format!("lifecycle {machine}");
        let (starts, key, kind, target) = match action {
            Action::Heal { handle } => {
                let (kind, target) = held[handle].clone();
                (false, handle.clone(), kind, target)
            }
            Action::Restart { machine }
            | Action::ResumeProcess { machine }
            | Action::Resume { machine }
            | Action::Reboot { machine } => {
                let (kind, _) = held[&lifecycle(machine)];
                (false, lifecycle(machine), kind, machine.clone())
            }
            Action::Partition { handle, left, .. } => {
                (true, handle.clone(), "partition", left[0].clone())
            }
            Action::Shape { handle, from, .. } => (true, handle.clone(), "shape", from[0].clone()),
            Action::ClockSkew {
                handle, machine, ..
            }
            | Action::ClockDrift {
                handle, machine, ..
            }
            | Action::Throttle {
                handle, machine, ..
            }
            | Action::DiskError {
                handle, machine, ..
            }
            | Action::DiskLatency {
                handle, machine, ..
            }
            | Action::Preempt {
                handle, machine, ..
            } => (true, handle.clone(), action.name(), machine.clone()),
            other => {
                let machine = other.machine().unwrap();
                (true, lifecycle(machine), other.name(), machine.to_owned())
            }
        };
        if !starts {
            held.remove(&key);
        }
        let others = held
            .values()
            .filter(|(_, machine)| *machine == target)
            .map(|(kind, _)| *kind)
            .collect();
        if starts {
            assert!(held.insert(key, (kind, target.clone())).is_none());
        }
        events.push(Step {
            starts,
            kind,
            others,
        });
    }
    events
}

/// Run `yaml` with `seed` to completion, as `applied` does for `PROFILE`.
fn run_profile(yaml: &str, seed: u64) -> Vec<(u64, Action)> {
    let mut world = Modelled::new(|_, _| false);
    let mut profile: nemesis::Profile = serde_yaml_ng::from_str(yaml).unwrap();
    profile.run(&mut world, &mut Rng::new(seed)).unwrap();
    assert_eq!(world.in_force(), 0, "every fault ends within run");
    world.applied
}

/// The domains of each kind, as `Fault::domains` gives them.
fn domains(kind: &str) -> Vec<Domain> {
    match kind {
        "crash" | "stop" | "pause" | "pause_process" => vec![Domain::Lifecycle],
        "partition" | "shape" => vec![Domain::Network],
        "clock_skew" => vec![Domain::Clock, Domain::Lifecycle],
        "clock_drift" => vec![Domain::Clock],
        "disk_error" => vec![Domain::DiskError],
        "disk_latency" => vec![Domain::DiskLatency],
        "throttle" | "preempt" => vec![Domain::Scheduler],
        other => panic!("{other}"),
    }
}

fn shares_a_domain(a: &str, b: &str) -> bool {
    domains(a).iter().any(|domain| domains(b).contains(domain))
}

#[test]
fn per_domain_overlap_starts_a_crash_while_a_disk_latency_holds_its_target() {
    let crash_in_latency = |yaml: &str| {
        (0..32)
            .flat_map(|seed| events(&run_profile(yaml, seed)))
            .filter(|e| e.starts && e.kind == "crash" && e.others.contains(&"disk_latency"))
            .count()
    };
    assert_eq!(crash_in_latency(SINGLE), 0);
    assert_eq!(crash_in_latency(&with_overlap(SINGLE, "per_target")), 0);
    assert!(crash_in_latency(&with_overlap(SINGLE, "per_domain")) > 0);
    // With one target, the default never holds two faults at once.
    for seed in 0..32 {
        assert!(events(&run_profile(SINGLE, seed))
            .iter()
            .all(|e| e.others.is_empty()));
    }
}

#[test]
fn ending_a_fault_frees_only_its_own_domain_in_either_order() {
    let single = with_overlap(SINGLE, "per_domain");
    // A latency ends during a crash's downtime, and another latency then
    // starts before the reboot; a reboot comes during a latency, and another
    // crash then starts while that latency still holds.
    let (mut latency_freed, mut crash_freed) = (0, 0);
    for seed in 0..200 {
        let events = events(&run_profile(&single, seed));
        for e in &events {
            // Never two faults of one domain on the target.
            assert!(
                e.others.iter().all(|other| !shares_a_domain(e.kind, other)),
                "seed {seed}: {e:?}"
            );
        }
        for (i, end) in events.iter().enumerate().filter(|(_, e)| !e.starts) {
            let (kind, holder) = match end.kind {
                "disk_latency" if end.others == ["crash"] => ("disk_latency", "crash"),
                "crash" if end.others == ["disk_latency"] => ("crash", "disk_latency"),
                _ => continue,
            };
            // The next event for the holder is its end; the freed kind may
            // start again before it.
            let rest = &events[i + 1..];
            let holder_ends = rest
                .iter()
                .position(|e| !e.starts && e.kind == holder)
                .expect("the holder ends within run");
            if rest[..holder_ends]
                .iter()
                .any(|e| e.starts && e.kind == kind && e.others == [holder])
            {
                match kind {
                    "disk_latency" => latency_freed += 1,
                    _ => crash_freed += 1,
                }
            }
        }
    }
    assert!(
        latency_freed > 0 && crash_freed > 0,
        "{latency_freed} {crash_freed}"
    );
}

/// One target, a clock_skew against a lifecycle fault and a clock fault.
const SKEW: &str = r#"
start_ns: 1000000000
end_ns: 18000000000
interval_ns: 1000000000
targets: [n1]
overlap: per_domain
faults:
  - {kind: clock_skew, weight: 2, duration_ns: {min: 1000000000, max: 2000000000},
     max_offset_ns: 5000000000}
  - {kind: crash, downtime_ns: {min: 500000000, max: 1000000000}}
  - {kind: clock_drift, duration_ns: {min: 500000000, max: 1000000000}, max_rate_ppb: 1000000}
"#;

#[test]
fn ending_a_clock_skew_frees_both_its_clock_and_lifecycle_slots() {
    // Nothing else holds the lifecycle or the clock slot while a skew does,
    // so a crash or a drift that starts after a skew ends, and before the
    // next skew, starts only because that end freed its slot.
    let (mut crash_after, mut drift_after) = (0, 0);
    for seed in 0..32 {
        let events = events(&run_profile(SKEW, seed));
        for (i, _) in events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.starts && e.kind == "clock_skew")
        {
            let rest = &events[i + 1..];
            let window = rest
                .iter()
                .position(|e| e.starts && e.kind == "clock_skew")
                .map_or(rest, |next| &rest[..next]);
            let starts = |kind: &str| window.iter().any(|e| e.starts && e.kind == kind);
            crash_after += usize::from(starts("crash"));
            drift_after += usize::from(starts("clock_drift"));
        }
    }
    assert!(
        crash_after > 0 && drift_after > 0,
        "{crash_after} {drift_after}"
    );
}

#[test]
fn per_domain_overlap_keeps_conflicting_faults_apart() {
    // Modelled refuses a second lifecycle fault on a machine, so a run
    // that overlaps two of them fails here.
    let profile = with_overlap(PROFILE, "per_domain");
    let mut across = 0;
    let mut kinds = BTreeSet::new();
    for seed in 0..100 {
        for e in events(&run_profile(&profile, seed)) {
            kinds.insert(e.kind);
            assert!(
                e.others.iter().all(|other| !shares_a_domain(e.kind, other)),
                "seed {seed}: {e:?}"
            );
            // A realtime step never meets a lifecycle fault: the engine
            // refuses the step, or its heal, on a paused or crashed VM.
            if e.kind == "clock_skew" {
                assert!(
                    e.others.iter().all(|o| domains(o) != [Domain::Lifecycle]),
                    "seed {seed}: {e:?}"
                );
            }
            if e.starts && !e.others.is_empty() {
                across += 1;
            }
        }
    }
    assert!(across > 0, "faults of different domains share a target");
    assert_eq!(kinds.len(), 12, "{kinds:?}");
}

#[test]
fn per_domain_overlap_keeps_the_fault_budget() {
    for (max, overlap) in [(3, "per_domain"), (3, "per_target"), (20, "per_domain")] {
        let yaml = with_overlap(&format!("{SINGLE}max_faults: {max}\n"), overlap);
        let starts: Vec<usize> = (0..32)
            .map(|seed| {
                let applied = run_profile(&yaml, seed);
                let starts = events(&applied).iter().filter(|e| e.starts).count();
                assert_eq!(applied.len(), 2 * starts);
                starts
            })
            .collect();
        assert!(starts.iter().all(|&n| n <= max), "{overlap}: {starts:?}");
        if max == 3 {
            assert!(starts.contains(&3), "{overlap}: {starts:?}");
        }
    }
    // The same stream, budget and gaps start more faults per domain: a
    // start is skipped only when its own domain is busy.
    let total = |yaml: &str| -> usize {
        (0..32)
            .map(|seed| {
                events(&run_profile(yaml, seed))
                    .iter()
                    .filter(|e| e.starts)
                    .count()
            })
            .sum()
    };
    assert!(total(&with_overlap(SINGLE, "per_domain")) > total(SINGLE));
}

#[test]
fn per_domain_overlap_heals_every_fault_after_a_refused_apply() {
    for attempt in [5, 9, 14] {
        let mut world = Modelled::new(move |n, _| n == attempt);
        let mut profile: nemesis::Profile =
            serde_yaml_ng::from_str(&with_overlap(PROFILE, "per_domain")).unwrap();
        let error = profile.run(&mut world, &mut Rng::new(7)).unwrap_err();
        assert!(error.0.ends_with("invalid_state: refused"), "{error}");
        profile.heal(&mut world).unwrap();
        assert_eq!(world.in_force(), 0, "attempt {attempt}");
        let applied = world.applied.len();
        profile.heal(&mut world).unwrap();
        assert_eq!(world.applied.len(), applied);
    }
    // One target holding a crash and a latency when the next apply is
    // refused: heal ends both.
    let single = with_overlap(SINGLE, "per_domain");
    let (seed, at) = (0..32)
        .find_map(|seed| {
            let events = events(&run_profile(&single, seed));
            let at = events
                .iter()
                .position(|e| e.starts && !e.others.is_empty())?;
            // The apply after the second fault starts.
            Some((seed, at + 2))
        })
        .expect("some seed overlaps");
    let mut world = Modelled::new(move |n, _| n == at);
    let mut profile: nemesis::Profile = serde_yaml_ng::from_str(&single).unwrap();
    profile.run(&mut world, &mut Rng::new(seed)).unwrap_err();
    assert_eq!(world.in_force(), 2, "seed {seed}");
    profile.heal(&mut world).unwrap();
    assert_eq!(world.in_force(), 0);
}
