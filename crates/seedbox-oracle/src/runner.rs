//! The runner that drives a [`Scenario`] through its phases.
use crate::{
    control::Action, random::Rng, Checkers, Client, Completion, Error, FinalRead, Read, Report,
    Scenario, Validity, Verdict, World,
};
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// Where verdicts go: the process's guest session, which the scenario's own
/// `seedbox_guest` calls share (so the final flush covers them too), or a
/// client of its own.
#[derive(Clone, Copy)]
enum Guest<'a> {
    Process,
    Client(&'a seedbox_guest::Client),
}

impl Guest<'_> {
    fn always(self, id: &str, ok: bool, details: Value) {
        match self {
            Self::Process => seedbox_guest::always(id, ok, details),
            Self::Client(c) => c.always(id, ok, details),
        }
    }
    fn flush(self) -> std::io::Result<()> {
        match self {
            Self::Process => seedbox_guest::flush(),
            Self::Client(c) => c.flush(),
        }
    }
}

/// Run `scenario` in the world, as its traits describe, reporting each
/// decided checker's verdict as a guest `always` property on the machine's
/// guest socket:
///
/// 1. [`Scenario::setup`]; a failure ends the run with every verdict
///    unknown.
/// 2. Run every client on its own thread, each with a generator split from
///    `random`, while [`crate::Nemesis::run`] runs on this one; stop the
///    clients once it returns, and wait for their last operations.
/// 3. [`crate::Nemesis::heal`], then the [`final_reads`].
/// 4. Check, evaluate each decided verdict's property (valid holds, invalid
///    violates; unknown evaluates nothing), flush.
///
/// A failed [`crate::Nemesis::run`] does not end the run: the clients
/// stop, and the heal, final reads and checks run as they would otherwise.
/// The [`Report`] keeps that error, a failed heal's error and each client
/// thread's panic beside the verdicts, and the runner prints a line naming
/// them. Any of them makes [`Report::exit_code`] report the run incomplete
/// unless a check was violated, whatever the verdicts say: the faults the
/// scenario describes did not all happen or did not all end, or a client
/// stopped early.
pub fn run<S: Scenario>(scenario: &mut S, world: &mut dyn World, random: &mut Rng) -> Report {
    drive_scenario(scenario, world, random, Guest::Process)
}

/// [`run`], reporting the verdicts through `guest`.
pub fn run_with<S: Scenario>(
    scenario: &mut S,
    world: &mut dyn World,
    random: &mut Rng,
    guest: &seedbox_guest::Client,
) -> Report {
    drive_scenario(scenario, world, random, Guest::Client(guest))
}

fn drive_scenario<S: Scenario>(
    scenario: &mut S,
    world: &mut dyn World,
    random: &mut Rng,
    guest: Guest<'_>,
) -> Report {
    let checkers = scenario.checkers();
    if let Err(error) = scenario.setup(world) {
        warn("setup", &error);
        let verdicts = unknown(&checkers, &format!("setup failed: {error}"));
        return finish(
            guest,
            Report {
                verdicts,
                ..Report::default()
            },
        );
    }
    let history = scenario.history();
    let clients = scenario.clients();
    let processes = clients.len() as u64;
    let stop = AtomicBool::new(false);
    let mut nemesis = scenario.nemesis();
    let mut report = Report::default();
    std::thread::scope(|scope| {
        let workers: Vec<_> = clients
            .into_iter()
            .enumerate()
            .map(|(process, client)| {
                let (history, stop, random) = (&*history, &stop, random.split());
                scope.spawn(move || drive(client, process as u64, processes, history, stop, random))
            })
            .collect();
        if let Err(error) = nemesis.run(world, random) {
            warn("nemesis", &error);
            report.nemesis = Some(error);
        }
        stop.store(true, Ordering::SeqCst);
        for (client, worker) in workers.into_iter().enumerate() {
            if let Err(panic) = worker.join() {
                report.panics.push((client, panic_message(panic.as_ref())));
            }
        }
    });
    if let Err(error) = nemesis.heal(world) {
        warn("heal", &error);
        report.heal = Some(error);
    }
    let mut reader = scenario.final_read();
    let reads = final_reads(reader.as_mut(), world);
    let records = history.records();
    report.verdicts = checkers
        .iter()
        .map(|checker| (checker.name().to_owned(), checker.check(&records, &reads)))
        .collect();
    finish(guest, report)
}

/// A panic's message, when it has one; the panic hook has already printed it
/// with its location.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
        (Some(message), _) => (*message).to_owned(),
        (_, Some(message)) => message.clone(),
        _ => "(no message)".to_owned(),
    }
}

/// One client's workload: generate, invoke and record until stopped. After
/// an `info` the client continues as a new process, as Jepsen's do.
fn drive<C: Client>(
    mut client: C,
    mut process: u64,
    processes: u64,
    history: &dyn crate::History<C::Op, C::Value>,
    stop: &AtomicBool,
    mut random: Rng,
) {
    while !stop.load(Ordering::SeqCst) {
        let Some(op) = client.generate(&mut random) else {
            return;
        };
        let invoke = history.invoke(process, op.clone());
        let completion = client.invoke(&op);
        let uncertain = matches!(completion, Completion::Info(_));
        history.complete(invoke, completion);
        if uncertain {
            process += processes.max(1);
        }
    }
}

/// Jepsen's final-read discipline, after the nemesis has healed: wait for
/// every node to serve; if one will not, restart the nodes whose process
/// exited on its own and wait again. Then poll every node's view once a
/// second until all agree for [`FinalRead::agree_for`], or
/// [`FinalRead::settle`] passes regardless, and read each node in order.
/// A node that could not be read keeps its error, which checkers treat as
/// unknown evidence.
pub fn final_reads<R>(reader: &mut dyn FinalRead<Read = R>, world: &mut dyn World) -> Vec<Read<R>> {
    let nodes = reader.nodes();
    if !reader.ready() {
        let exited: Vec<_> = nodes
            .iter()
            .filter(|node| reader.exited(node))
            .cloned()
            .collect();
        for machine in exited {
            if let Err(error) = world.apply(&Action::Restart {
                machine: machine.clone(),
            }) {
                eprintln!("seedbox-oracle: restart {machine}: {error}");
            }
        }
        // Readiness is the readers' own check; reading reports what is left.
        reader.ready();
    }
    settle(reader, world, &nodes);
    nodes
        .into_iter()
        .map(|node| Read {
            result: reader.read(&node),
            node,
        })
        .collect()
}

/// Poll views until they agree for `agree_for` or `settle` passes. Times are
/// the world's simulated time.
fn settle<R>(reader: &mut dyn FinalRead<Read = R>, world: &mut dyn World, nodes: &[String]) {
    let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    let (settle, agree_for) = (nanos(reader.settle()), nanos(reader.agree_for()));
    let Ok(start) = world.now_ns() else { return };
    let mut since: Option<u64> = None;
    loop {
        let views: Vec<Value> = nodes.iter().map(|n| reader.view(n)).collect();
        let Ok(now) = world.now_ns() else { return };
        if views.windows(2).all(|pair| pair[0] == pair[1]) {
            if now.saturating_sub(*since.get_or_insert(now)) >= agree_for {
                return;
            }
        } else {
            since = None;
        }
        if now.saturating_sub(start) >= settle || world.sleep(Duration::from_secs(1)).is_err() {
            return;
        }
    }
}

fn unknown<O, V, R>(checkers: &Checkers<O, V, R>, why: &str) -> Vec<(String, Verdict)> {
    checkers
        .iter()
        .map(|checker| {
            let verdict = Verdict {
                validity: Validity::Unknown,
                summary: why.into(),
                details: Value::Null,
            };
            (checker.name().to_owned(), verdict)
        })
        .collect()
}

fn warn(phase: &str, error: &Error) {
    eprintln!("seedbox-oracle: {phase} failed: {error}");
}

/// Evaluate each decided checker's property and flush, so the verdicts are
/// part of the run's results before the oracle exits. Print each verdict,
/// then one line naming the steps that failed, if any did.
fn finish(guest: Guest<'_>, report: Report) -> Report {
    for (name, verdict) in &report.verdicts {
        println!("{name}: {:?}: {}", verdict.validity, verdict.summary);
        if let Some(condition) = verdict.condition() {
            let details = json!({"summary": verdict.summary, "details": verdict.details});
            guest.always(name, condition, details);
        }
    }
    let failures = report.failures();
    if !failures.is_empty() {
        println!("incomplete: {}", failures.join("; "));
    }
    if let Err(error) = guest.flush() {
        eprintln!("seedbox-oracle: property flush failed: {error}");
    }
    report
}

/// A client type for scenarios whose operations happen elsewhere and
/// [`Scenario::clients`] is empty, such as a workload on a client machine
/// streaming its history to the oracle. It has no values.
pub struct NoClient<O, V>(
    std::convert::Infallible,
    std::marker::PhantomData<fn() -> (O, V)>,
);

impl<O: Clone + Send + 'static, V: Send + 'static> Client for NoClient<O, V> {
    type Op = O;
    type Value = V;
    fn generate(&mut self, _: &mut dyn crate::Random) -> Option<O> {
        match self.0 {}
    }
    fn invoke(&mut self, _: &O) -> Completion<V> {
        match self.0 {}
    }
}

/// Run `scenario` inside a Seedbox guest: connect the oracle's world, seed
/// the generator from the guest's entropy, run, and return the oracle's
/// exit status ([`Report::exit_code`]).
pub fn main<S: Scenario>(scenario: &mut S) -> std::process::ExitCode {
    let mut world = match crate::world::GuestWorld::connect() {
        Ok(world) => world,
        Err(error) => {
            eprintln!("seedbox-oracle: {error}");
            return std::process::ExitCode::from(Report::UNKNOWN);
        }
    };
    let report = run(scenario, &mut world, &mut Rng::from_entropy());
    std::process::ExitCode::from(report.exit_code())
}

#[cfg(test)]
mod tests;
