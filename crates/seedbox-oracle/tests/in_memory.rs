//! A fake replicated log run through every trait, as a runner would drive it.
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
