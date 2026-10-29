//! A fake replicated log run through every trait, as a runner would drive it,
//! and a fault profile run against a world that models every fault in force.
use seedbox_oracle::*;
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
    };
    assert_eq!(undecided.exit_code(), Report::UNKNOWN);
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
  - {kind: shape, duration_ns: {min: 500000000, max: 3000000000},
     latency_ns: {min: 1000000, max: 50000000}, jitter_ns: 500000, to: [n1, n2, n3, client]}
  - {kind: crash, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: pause, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: pause_process, duration_ns: {min: 500000000, max: 3000000000}}
  - {kind: stop, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: clock_skew, duration_ns: {min: 500000000, max: 3000000000}, max_offset_ns: 5000000000}
  - {kind: clock_drift, duration_ns: {min: 500000000, max: 3000000000}, max_rate_ppb: 1000000}
  - {kind: throttle, duration_ns: {min: 500000000, max: 3000000000}, factor_permille: 4000}
  - {kind: disk_error, duration_ns: {min: 500000000, max: 3000000000}, op: write, ppm: 1000,
     targets: [n1, n2]}
  - {kind: disk_latency, duration_ns: {min: 500000000, max: 3000000000}, op: flush,
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
    let kinds: BTreeSet<_> = (0..20)
        .flat_map(applied)
        .map(|(_, action)| action.name())
        .collect();
    assert_eq!(kinds.len(), 17, "{kinds:?}");
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
