//! Fault-profile acceptance oracle. Each node (`node`) answers `ping` with
//! `pong` on port 7000. The oracle (`oracle`) pings random nodes while the
//! fault profile in `profile.yaml` runs as its nemesis, heals, then reads
//! every node. It prints every action the world applied, and two checks
//! decide the run: every node serves after the heal, and the profile ran
//! without a refusal.
use seedbox_oracle::{
    nemesis::Profile, Checker, Checkers, Client, Completion, Error, FinalRead, History, Nemesis,
    Random, RandomExt, Read, Record, Recorder, Scenario, Validity, Verdict, World,
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const NODES: [&str; 3] = ["n1", "n2", "n3"];
const PORT: u16 = 7000;
const PROFILE: &str = include_str!("../profile.yaml");

fn node() -> ExitCode {
    let listener = TcpListener::bind(("0.0.0.0", PORT)).expect("bind");
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                if (&stream).write_all(b"pong\n").is_err() {
                    break;
                }
                line.clear();
            }
        });
    }
    ExitCode::SUCCESS
}

/// Ask `node` for a pong. A refused connection certainly did nothing.
fn ping(node: &str) -> Completion<()> {
    let timeout = Duration::from_millis(500);
    let Some(address) = (node, PORT)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    else {
        return Completion::Fail(format!("{node}: no address"));
    };
    let stream = match TcpStream::connect_timeout(&address, timeout) {
        Ok(stream) => stream,
        Err(error) => return Completion::Fail(format!("{node}: connect: {error}")),
    };
    let exchange = || -> std::io::Result<String> {
        stream.set_read_timeout(Some(timeout))?;
        (&stream).write_all(b"ping\n")?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        Ok(line)
    };
    match exchange() {
        Ok(line) if line == "pong\n" => Completion::Ok(()),
        Ok(line) => Completion::Info(format!("{node}: answered {line:?}")),
        Err(error) => Completion::Info(format!("{node}: {error}")),
    }
}

struct Pinger;

impl Client for Pinger {
    type Op = usize;
    type Value = ();
    fn generate(&mut self, random: &mut dyn Random) -> Option<usize> {
        std::thread::sleep(Duration::from_millis(100));
        Some(random.below(NODES.len() as u64) as usize)
    }
    fn invoke(&mut self, op: &usize) -> Completion<()> {
        ping(NODES[*op])
    }
}

/// What the profile applied and any failure it reported.
#[derive(Default)]
struct Outcome {
    applied: usize,
    failures: Vec<String>,
}

/// The world, printing every action it applies.
struct Printing<'a> {
    world: &'a mut dyn World,
    outcome: &'a Mutex<Outcome>,
}

impl World for Printing<'_> {
    fn now_ns(&mut self) -> Result<u64, Error> {
        self.world.now_ns()
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        self.world.sleep(duration)
    }
    fn apply(&mut self, action: &seedbox_oracle::Action) -> Result<(), Error> {
        let result = self.world.apply(action);
        let now = self.world.now_ns().unwrap_or(0);
        let action = serde_json::to_string(action).expect("actions serialize");
        match &result {
            Ok(()) => {
                println!("applied at {now}: {action}");
                self.outcome.lock().unwrap().applied += 1;
            }
            Err(error) => println!("refused at {now}: {action}: {error}"),
        }
        result
    }
}

/// The profile, with its outcome kept for the checks.
struct Logged {
    profile: Profile,
    outcome: Arc<Mutex<Outcome>>,
}

impl Logged {
    fn record(&self, phase: &str, result: Result<(), Error>) -> Result<(), Error> {
        if let Err(error) = &result {
            let failure = format!("{phase}: {error}");
            self.outcome.lock().unwrap().failures.push(failure);
        }
        result
    }
}

impl Nemesis for Logged {
    fn run(&mut self, world: &mut dyn World, random: &mut dyn Random) -> Result<(), Error> {
        let outcome = self.outcome.clone();
        let mut world = Printing {
            world,
            outcome: &outcome,
        };
        let result = self.profile.run(&mut world, random);
        self.record("run", result)
    }
    fn heal(&mut self, world: &mut dyn World) -> Result<(), Error> {
        let outcome = self.outcome.clone();
        let mut world = Printing {
            world,
            outcome: &outcome,
        };
        let result = self.profile.heal(&mut world);
        self.record("heal", result)
    }
}

struct Reads;

impl FinalRead for Reads {
    type Read = ();
    fn nodes(&self) -> Vec<String> {
        NODES.iter().map(|n| n.to_string()).collect()
    }
    fn ready(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if NODES.iter().all(|n| matches!(ping(n), Completion::Ok(()))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }
    fn exited(&mut self, _: &str) -> bool {
        false
    }
    fn view(&mut self, _: &str) -> Value {
        Value::Null
    }
    fn read(&mut self, node: &str) -> Result<(), String> {
        match ping(node) {
            Completion::Ok(()) => Ok(()),
            Completion::Fail(error) | Completion::Info(error) => Err(error),
        }
    }
    fn settle(&self) -> Duration {
        Duration::ZERO
    }
}

fn verdict(holds: bool, summary: String, details: Value) -> Verdict {
    let validity = if holds {
        Validity::Valid
    } else {
        Validity::Invalid
    };
    Verdict {
        validity,
        summary,
        details,
    }
}

struct Serves;

impl Checker<usize, (), ()> for Serves {
    fn name(&self) -> &str {
        "every node serves after the profile heals"
    }
    fn check(&self, history: &[Record<usize, ()>], reads: &[Read<()>]) -> Verdict {
        let unread: Vec<_> = reads
            .iter()
            .filter_map(|r| {
                r.result
                    .as_ref()
                    .err()
                    .map(|e| json!({"node": r.node, "error": e}))
            })
            .collect();
        let summary = format!("{} records, {} nodes unread", history.len(), unread.len());
        verdict(unread.is_empty(), summary, json!({ "unread": unread }))
    }
}

struct Ran(Arc<Mutex<Outcome>>);

impl Checker<usize, (), ()> for Ran {
    fn name(&self) -> &str {
        "the fault profile runs and heals without a refusal"
    }
    fn check(&self, _: &[Record<usize, ()>], _: &[Read<()>]) -> Verdict {
        let outcome = self.0.lock().unwrap();
        let summary = format!("{} actions applied", outcome.applied);
        let holds = outcome.failures.is_empty() && outcome.applied > 0;
        verdict(holds, summary, json!({ "failures": outcome.failures }))
    }
}

struct Profiled {
    history: Arc<Recorder<usize, ()>>,
    outcome: Arc<Mutex<Outcome>>,
}

impl Scenario for Profiled {
    type Op = usize;
    type Value = ();
    type Read = ();
    type Client = Pinger;
    fn name(&self) -> &str {
        "fault-profile"
    }
    fn setup(&mut self, _: &mut dyn World) -> Result<(), Error> {
        if Reads.ready() {
            Ok(())
        } else {
            Err("the nodes never served".into())
        }
    }
    fn history(&self) -> Arc<dyn History<usize, ()>> {
        self.history.clone()
    }
    fn clients(&mut self) -> Vec<Pinger> {
        vec![Pinger, Pinger]
    }
    fn nemesis(&mut self) -> Box<dyn Nemesis> {
        let profile: Profile = serde_yaml_ng::from_str(PROFILE).expect("profile.yaml parses");
        Box::new(Logged {
            profile,
            outcome: self.outcome.clone(),
        })
    }
    fn final_read(&mut self) -> Box<dyn FinalRead<Read = ()>> {
        Box::new(Reads)
    }
    fn checkers(&self) -> Checkers<usize, (), ()> {
        vec![Box::new(Serves), Box::new(Ran(self.outcome.clone()))]
    }
}

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("node") => node(),
        Some("oracle") => seedbox_oracle::main(&mut Profiled {
            history: Arc::new(Recorder::new()),
            outcome: Arc::default(),
        }),
        _ => {
            eprintln!("usage: profile-oracle node|oracle");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[test]
fn the_shipped_profile_is_valid() {
    let profile: Profile = serde_yaml_ng::from_str(PROFILE).unwrap();
    profile.validate().unwrap();
}
