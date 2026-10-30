use super::*;
use crate::{Checker, Event, FinalRead, History, Nemesis, Random, Record, Recorder, World};
use std::{
    collections::BTreeSet,
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};

const NODES: [&str; 3] = ["n1", "n2", "n3"];
const OPS: u64 = 5;

/// Three replicas of one log; appends reach every replica not cut off, and
/// a heal repairs cut-off replicas from n1 unless `lossy`.
#[derive(Default)]
struct Cluster {
    logs: [Vec<u64>; 3],
    cut: BTreeSet<usize>,
    lossy: bool,
    appended: u64,
    /// The nemesis has cut the cluster: clients start only then, so every
    /// run sees the fault.
    open: bool,
}

#[derive(Default)]
struct Shared {
    cluster: Mutex<Cluster>,
    progress: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Cluster> {
        self.cluster.lock().unwrap()
    }
}

/// Appends `OPS` unique values `process * 1000 + n`, then ends its workload.
struct Appender {
    shared: Arc<Shared>,
    id: u64,
    sent: u64,
}

impl Client for Appender {
    type Op = u64;
    type Value = usize;
    fn generate(&mut self, random: &mut dyn Random) -> Option<u64> {
        random.next_u64();
        self.sent += 1;
        (self.sent <= OPS).then_some(self.id * 1000 + self.sent)
    }
    fn invoke(&mut self, op: &u64) -> Completion<usize> {
        let mut cluster = self.shared.lock();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !cluster.open {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the nemesis never cut the cluster");
            cluster = self.shared.progress.wait_timeout(cluster, left).unwrap().0;
        }
        let cut = cluster.cut.clone();
        for (i, log) in cluster.logs.iter_mut().enumerate() {
            if !cut.contains(&i) {
                log.push(*op);
            }
        }
        cluster.appended += 1;
        let length = cluster.logs[0].len();
        drop(cluster);
        self.shared.progress.notify_all();
        // Every third op times out after taking effect.
        if op.is_multiple_of(3) {
            Completion::Info("timeout".into())
        } else {
            Completion::Ok(length)
        }
    }
}

/// A world whose sleeps wait for every client's workload to finish: a
/// positive signal, bounded only by a generous containment timeout.
struct FakeWorld {
    shared: Arc<Shared>,
    now_ns: u64,
    applied: Vec<Action>,
    clients: u64,
}

impl World for FakeWorld {
    fn now_ns(&mut self) -> Result<u64, Error> {
        Ok(self.now_ns)
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut cluster = self.shared.lock();
        while cluster.appended < self.clients * OPS {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "clients never finished");
            cluster = self.shared.progress.wait_timeout(cluster, left).unwrap().0;
        }
        self.now_ns += duration.as_nanos() as u64;
        Ok(())
    }
    fn apply(&mut self, action: &Action) -> Result<(), Error> {
        self.applied.push(action.clone());
        let mut cluster = self.shared.lock();
        match action {
            Action::Partition { left, .. } => {
                for node in left {
                    let index = NODES.iter().position(|n| n == node).unwrap();
                    cluster.cut.insert(index);
                }
                cluster.open = true;
                self.shared.progress.notify_all();
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
            Action::Restart { .. } => {}
            other => return Err(format!("unsupported {}", other.name()).into()),
        }
        Ok(())
    }
}

/// Cuts n3 off, waits, and leaves the cut for `heal`.
#[derive(Default)]
struct Isolate(crate::patterns::InForce);

impl Nemesis for Isolate {
    fn run(&mut self, world: &mut dyn World, _: &mut dyn Random) -> Result<(), Error> {
        let handle = self.0.name("isolate");
        self.0.apply(
            world,
            &Action::Partition {
                handle,
                left: vec!["n3".into()],
                right: vec!["n1".into(), "n2".into()],
            },
        )?;
        world.sleep(Duration::from_secs(1))
    }
    fn heal(&mut self, world: &mut dyn World) -> Result<(), Error> {
        self.0.heal(world)
    }
}

struct Reads(Arc<Shared>);

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
        let index = NODES.iter().position(|n| *n == node).ok_or("no node")?;
        Ok(self.0.lock().logs[index].clone())
    }
    fn agree_for(&self) -> Duration {
        Duration::ZERO
    }
}

/// Every invoked append is on every replica.
struct Survives;

impl Checker<u64, usize, Vec<u64>> for Survives {
    fn name(&self) -> &str {
        "appends survive on every replica"
    }
    fn check(&self, history: &[Record<u64, usize>], reads: &[Read<Vec<u64>>]) -> Verdict {
        let invoked: BTreeSet<u64> = history
            .iter()
            .filter_map(|r| match r.event {
                Event::Invoke { op } => Some(op),
                _ => None,
            })
            .collect();
        let missing: usize = reads
            .iter()
            .map(|read| {
                let held: BTreeSet<u64> = read.result.clone().unwrap().into_iter().collect();
                invoked.difference(&held).count()
            })
            .sum();
        Verdict {
            validity: if missing == 0 {
                Validity::Valid
            } else {
                Validity::Invalid
            },
            summary: format!("{missing} missing"),
            details: json!({ "invoked": invoked.len(), "missing": missing }),
        }
    }
}

struct Toy {
    shared: Arc<Shared>,
    history: Arc<Recorder<u64, usize>>,
    fail_setup: bool,
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
        if self.fail_setup {
            return Err("no leader".into());
        }
        Ok(())
    }
    fn history(&self) -> Arc<dyn History<u64, usize>> {
        self.history.clone()
    }
    fn clients(&mut self) -> Vec<Appender> {
        (0..2)
            .map(|id| Appender {
                shared: self.shared.clone(),
                id,
                sent: 0,
            })
            .collect()
    }
    fn nemesis(&mut self) -> Box<dyn Nemesis> {
        Box::new(Isolate::default())
    }
    fn final_read(&mut self) -> Box<dyn FinalRead<Read = Vec<u64>>> {
        Box::new(Reads(self.shared.clone()))
    }
    fn checkers(&self) -> Checkers<u64, usize, Vec<u64>> {
        vec![Box::new(Survives)]
    }
}

fn outcome(lossy: bool, fail_setup: bool) -> (Report, FakeWorld, Vec<Record<u64, usize>>) {
    let shared = Arc::new(Shared::default());
    shared.lock().lossy = lossy;
    let history = Arc::new(Recorder::with_clock(|| 0));
    let mut scenario = Toy {
        shared: shared.clone(),
        history: history.clone(),
        fail_setup,
    };
    let mut world = FakeWorld {
        shared,
        now_ns: 0,
        applied: Vec::new(),
        clients: 2,
    };
    let report = run(&mut scenario, &mut world, &mut Rng::new(7));
    (report, world, history.records())
}

#[test]
fn clients_run_concurrently_until_the_nemesis_returns_then_everything_heals() {
    let (report, world, records) = outcome(false, false);
    assert_eq!(report.exit_code(), Report::OK, "{report:?}");
    assert_eq!(report.verdicts[0].0, "appends survive on every replica");
    assert_eq!(report.verdicts[0].1.details["invoked"], 2 * OPS);
    let [Action::Partition { handle, .. }, Action::Heal { handle: healed }] = &world.applied[..]
    else {
        panic!("{:?}", world.applied);
    };
    assert_eq!(handle, healed);
    // Every invocation completes once; after an info its client continues
    // as process + 2.
    assert_eq!(records.len() as u64, 4 * OPS);
    let processes: BTreeSet<u64> = records.iter().map(|r| r.process).collect();
    assert!(processes.contains(&0) && processes.contains(&1));
    assert!(processes.iter().any(|p| *p >= 2), "{processes:?}");
    for record in &records {
        if let Event::Ok { invoke, .. } | Event::Info { invoke, .. } = record.event {
            assert_eq!(records[invoke as usize].process, record.process);
        }
    }
}

#[test]
fn appends_a_heal_cannot_repair_are_a_violation() {
    let (report, _, _) = outcome(true, false);
    assert_eq!(report.exit_code(), Report::INVALID);
    assert_eq!(report.verdicts[0].1.condition(), Some(false));
}

#[test]
fn a_failed_setup_leaves_every_verdict_unknown() {
    let (report, world, records) = outcome(false, true);
    assert_eq!(report.exit_code(), Report::UNKNOWN);
    assert_eq!(report.verdicts[0].1.summary, "setup failed: no leader");
    assert!(world.applied.is_empty() && records.is_empty());
}

/// Views converge after a few polls; one node exited and needs a restart.
struct Converging {
    polls: u32,
    ready: bool,
}

impl FinalRead for Converging {
    type Read = u32;
    fn nodes(&self) -> Vec<String> {
        vec!["a".into(), "b".into()]
    }
    fn ready(&mut self) -> bool {
        std::mem::replace(&mut self.ready, true)
    }
    fn exited(&mut self, node: &str) -> bool {
        node == "b"
    }
    fn view(&mut self, node: &str) -> Value {
        self.polls += 1;
        json!(if node == "b" && self.polls < 7 { 0 } else { 1 })
    }
    fn read(&mut self, node: &str) -> Result<u32, String> {
        match node {
            "a" => Ok(self.polls),
            _ => Err("refused".into()),
        }
    }
    fn agree_for(&self) -> Duration {
        Duration::from_secs(2)
    }
}

#[test]
fn final_reads_restart_exited_nodes_and_wait_for_agreement() {
    let mut world = FakeWorld {
        shared: Arc::new(Shared::default()),
        now_ns: 0,
        applied: Vec::new(),
        clients: 0,
    };
    let mut reader = Converging {
        polls: 0,
        ready: false,
    };
    let reads = final_reads(&mut reader, &mut world);
    assert_eq!(
        world.applied,
        [Action::Restart {
            machine: "b".into()
        }]
    );
    // Disagreeing until the fourth poll, then agreeing for two seconds.
    assert_eq!(world.now_ns, 5_000_000_000);
    assert_eq!(
        reads[0],
        Read {
            node: "a".into(),
            result: Ok(12)
        }
    );
    assert_eq!(reads[1].result, Err("refused".to_string()));
}

#[test]
#[cfg(target_os = "linux")]
fn verdicts_are_reported_as_properties_and_flushed() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let directory = tempfile::Builder::new()
        .prefix("sbx-run-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = directory.path().join("guest.sock");
    // SAFETY: plain socket calls on descriptors this test owns.
    let listener = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0);
        assert!(fd >= 0);
        let mut address: libc::sockaddr_un = std::mem::zeroed();
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, byte) in address
            .sun_path
            .iter_mut()
            .zip(path.to_str().unwrap().bytes())
        {
            *target = byte as libc::c_char;
        }
        let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        assert_eq!(
            libc::bind(fd, (&address as *const libc::sockaddr_un).cast(), size),
            0
        );
        assert_eq!(libc::listen(fd, 1), 0);
        OwnedFd::from_raw_fd(fd)
    };
    let supervisor = std::thread::spawn(move || {
        // SAFETY: accept/recv/send on owned descriptors with valid buffers.
        unsafe {
            let session = libc::accept(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert!(session >= 0);
            let session = OwnedFd::from_raw_fd(session);
            let mut records = Vec::new();
            loop {
                let mut buffer = [0u8; 4096];
                let n = libc::recv(
                    session.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                );
                assert!(n > 0, "session ended before its flush");
                let record: Value = serde_json::from_slice(&buffer[..n as usize]).unwrap();
                records.push(record.clone());
                if record.get("flush").is_some() {
                    let reply = format!(r#"{{"flush":{{"seq":{}}}}}"#, records.len());
                    libc::send(session.as_raw_fd(), reply.as_ptr().cast(), reply.len(), 0);
                    return records;
                }
            }
        }
    });
    let shared = Arc::new(Shared::default());
    shared.lock().lossy = true;
    let mut scenario = Toy {
        shared: shared.clone(),
        history: Arc::new(Recorder::with_clock(|| 0)),
        fail_setup: false,
    };
    let mut world = FakeWorld {
        shared,
        now_ns: 0,
        applied: Vec::new(),
        clients: 2,
    };
    let guest = seedbox_guest::Client::new(&path);
    let report = run_with(&mut scenario, &mut world, &mut Rng::new(7), &guest);
    assert_eq!(report.exit_code(), Report::INVALID);
    let records = supervisor.join().unwrap();
    let name = "appends survive on every replica";
    // Only the decided verdict's evaluation, then the flush.
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["assert"]["id"], name);
    assert_eq!(records[0]["assert"]["kind"], "always");
    assert_eq!(records[0]["assert"]["condition"], false);
    assert!(records[0]["assert"]["details"]["summary"].is_string());
    assert_eq!(records[1], json!({"flush": {}}));
}
