//! Rule C3 on a native machine: a stop that delivers nothing leaves no trace.
//!
//! A backend boots a guest whose workload runs two loops that read the clock
//! and sleep, printing [`DONE`] when each ends. [`stops_leave_no_trace`]
//! drives it once with the machine's own stops and host barriers every half
//! millisecond, and again per seed with the same barriers and runs that end
//! early at seeded times: a few microseconds apart, and nanoseconds apart
//! from boot and after each wake, where a guest's clock starts and where it
//! goes idle. At every barrier the native witness and the console must agree,
//! and so must the full state evidence wherever the backend takes it.
//!
//! The backend supplies the machine, its console and how often state
//! evidence is worth its cost; guest setup and backend probes stay with it.
use super::SeededRng;
use crate::{
    clock::{Clock, SimulationClock},
    execution::ExecutionConfig,
    machine::{ExecutionEvidence, Machine, RunContext, StateEvidence, StopReason, Timestamp},
    rng::RngStreamExt,
};
use std::{collections::VecDeque, time::Duration};

const MS: u64 = 1_000_000;
/// Host barriers, shared by every schedule.
pub const BARRIER: u64 = MS / 2;
/// The longest run of a split schedule.
const SPLIT: u64 = 50_000;
/// Split runs that end at most 4 ns after their start, from boot and after
/// each wake.
const TINY: u32 = 16;
/// Simulated time after both loops are done, so the idle guest is compared too.
const AFTER: u64 = 3 * MS;
/// Bound on the simulated length of boot and the workload.
const LIMIT: u64 = 20_000 * MS;
/// What the guest's two loops print on the console when they end.
pub const DONE: [&str; 2] = ["C3_PARENT_DONE", "C3_CHILD_DONE"];

/// What a backend supplies to the scenario.
pub struct Backend<'a, M> {
    /// Names the backend in a hang report.
    pub name: &'a str,
    /// Bounds a hung guest; never a pass condition.
    pub hang: Duration,
    /// A fresh machine with a policy that has a witness and full state.
    pub machine: &'a dyn Fn() -> M,
    /// The machine's console so far.
    pub console: &'a dyn Fn(&mut M) -> Vec<u8>,
    /// Full state evidence is taken at every this many barriers, and the last.
    pub state_every: usize,
    /// CPU-work pricing of every run up to barrier `index`, counted from 1.
    /// Every schedule shares the barriers, so pricing changes only where all
    /// of them stop.
    pub execution: &'a dyn Fn(usize) -> ExecutionConfig,
}

/// What the machine shows at one barrier.
#[derive(PartialEq, Eq)]
pub struct Look {
    pub at: u64,
    pub console: Vec<u8>,
    pub witness: Option<ExecutionEvidence>,
    pub state: Option<StateEvidence>,
}

/// Where two looks differ, in terms a failure can be read by: the state
/// evidence parts and the guest physical pages that differ.
pub fn differences(own: &Look, other: &Look) -> String {
    let mut found = Vec::new();
    if own.console != other.console {
        found.push(format!(
            "console:\n  own:   {}\n  other: {}",
            String::from_utf8_lossy(&own.console),
            String::from_utf8_lossy(&other.console)
        ));
    }
    if own.witness != other.witness {
        found.push(format!(
            "witness: own {:?}, other {:?}",
            own.witness, other.witness
        ));
    }
    if let (Some(a), Some(b)) = (&own.state, &other.state) {
        for ((name, x), (_, y)) in a.parts.iter().zip(&b.parts) {
            if x != y {
                found.push(format!("state part {name}"));
            }
        }
        let mut pages = Vec::new();
        let mut index = 0;
        for region in &a.regions {
            for page in 0..region.length / 4096 {
                let (x, y) = (
                    a.page_hashes.get(index * 8..index * 8 + 8),
                    b.page_hashes.get(index * 8..index * 8 + 8),
                );
                if x != y {
                    pages.push(format!("{:#x}", region.base + page * 4096));
                }
                index += 1;
            }
        }
        if !pages.is_empty() {
            found.push(format!("{} pages: {}", pages.len(), pages.join(" ")));
        }
    }
    found.join("\n")
}

fn run<M>(
    machine: &mut M,
    clock: &SimulationClock,
    rng: &mut SeededRng,
    execution: ExecutionConfig,
    upto: u64,
) -> StopReason
where
    M: Machine<Context = ()>,
    M::Error: std::fmt::Debug,
{
    let at = Timestamp::from_nanos;
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    machine
        .run(
            &mut RunContext {
                worker: &mut (),
                clock,
                rng,
                execution,
                inbox: crate::pv::Inbox::new(&mut inbox, at(clock.now())),
                outbox: crate::pv::Outbox::new(&mut outputs, at(clock.now()), at(upto)),
            },
            at(upto),
        )
        .unwrap()
}

/// Drive a fresh machine through `barriers` (or, without them, until both
/// loops are done and `AFTER` more), looking at each. A blocked machine
/// sleeps until its deadline, as the Driver wakes it. With a `split` seed,
/// most runs end instead at a time drawn in (now, next barrier], at most
/// `SPLIT` after now, or at most 4 ns after now for `TINY` runs from boot and
/// from each wake. Returns the looks and how many runs the machine took.
pub fn drive<M>(
    backend: &Backend<'_, M>,
    barriers: Option<&[u64]>,
    split: Option<u64>,
) -> (Vec<Look>, usize)
where
    M: Machine<Context = ()>,
    M::Error: std::fmt::Debug,
{
    let mut machine = (backend.machine)();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(7);
    let mut stops = split.map(SeededRng::new);
    let guard = seedbox_test_time::HangGuard::new(
        format!("{} stop-invariance guest", backend.name),
        backend.hang,
    );
    let mut looks = Vec::new();
    let mut runs = 0;
    let mut blocked: Option<Option<u64>> = None;
    let mut done_at = None;
    // Runs left that end within a few nanoseconds of their start: from boot,
    // where the guest's clock starts before it can stop, and after each wake,
    // where the guest takes its timer and goes idle within nanoseconds.
    let mut tiny = TINY;
    for index in 1.. {
        let (target, last) = match barriers {
            Some(barriers) => match barriers.get(index - 1) {
                Some(&target) => (target, index == barriers.len()),
                None => break,
            },
            None => (index as u64 * BARRIER, false),
        };
        if barriers.is_none() && done_at.is_some_and(|done| target > done + AFTER) {
            break;
        }
        assert!(
            target <= LIMIT,
            "guest never finished: {}",
            String::from_utf8_lossy(&(backend.console)(&mut machine))
        );
        let execution = (backend.execution)(index);
        loop {
            guard.remaining().expect("stop-invariance hang");
            if let Some(deadline) = blocked {
                match deadline {
                    Some(deadline) if deadline <= target => {
                        clock.set(deadline);
                        blocked = None;
                        tiny = TINY;
                    }
                    _ => {
                        clock.set(target);
                        break;
                    }
                }
            }
            let now = clock.now();
            let split = now < target
                && stops
                    .as_mut()
                    .is_some_and(|stops| stops.next_u32() % 8 != 0);
            let upto = match &mut stops {
                Some(stops) if split && tiny > 0 => {
                    tiny -= 1;
                    now + 1 + stops.next_u64() % (target - now).min(4)
                }
                Some(stops) if split => now + 1 + stops.next_u64() % (target - now).min(SPLIT),
                _ => target,
            };
            runs += 1;
            match run(&mut machine, &clock, &mut rng, execution, upto) {
                StopReason::BoundaryReached => {
                    clock.set(upto);
                    if upto == target {
                        break;
                    }
                }
                StopReason::Blocked { deadline, .. } => {
                    blocked = Some(deadline.map(|deadline| deadline.as_nanos()));
                }
                other => panic!(
                    "unexpected stop {other:?}: {}",
                    String::from_utf8_lossy(&(backend.console)(&mut machine))
                ),
            }
        }
        let console = (backend.console)(&mut machine);
        let text = String::from_utf8_lossy(&console);
        assert!(!text.contains("Kernel panic"), "{text}");
        if done_at.is_none() && DONE.iter().all(|done| text.contains(done)) {
            done_at = Some(target);
        }
        // The own schedule's last barrier is only known when it ends, so it
        // takes state there when it finds itself done, and a split schedule
        // at its last.
        let finishing =
            barriers.is_none() && done_at.is_some_and(|done| target + BARRIER > done + AFTER);
        let state = if index % backend.state_every == 0 || last || finishing {
            machine.state_evidence().unwrap()
        } else {
            None
        };
        looks.push(Look {
            at: target,
            console,
            witness: machine.execution_evidence().unwrap(),
            state,
        });
    }
    (looks, runs)
}

/// Assert that the backend's guest shows the same witness, console and state
/// at every barrier under its own stops and under each split seed.
pub fn stops_leave_no_trace<M>(backend: &Backend<'_, M>, seeds: &[u64])
where
    M: Machine<Context = ()>,
    M::Error: std::fmt::Debug,
{
    let (own, own_runs) = drive(backend, None, None);
    let text = String::from_utf8_lossy(&own.last().unwrap().console).into_owned();
    assert!(DONE.iter().all(|done| text.contains(done)), "{text}");
    assert!(
        own.iter().all(|look| look.witness.is_some()),
        "policy witness"
    );
    assert!(
        own.iter()
            .enumerate()
            .all(|(index, look)| (index + 1) % backend.state_every != 0 || look.state.is_some())
            && own.last().is_some_and(|look| look.state.is_some()),
        "full state evidence"
    );
    let barriers: Vec<u64> = own.iter().map(|look| look.at).collect();
    for &seed in seeds {
        let (split, runs) = drive(backend, Some(&barriers), Some(seed));
        assert!(
            runs > own_runs,
            "seed {seed} split runs: {runs} against {own_runs}"
        );
        for (a, b) in own.iter().zip(&split) {
            assert!(
                a == b,
                "seed {seed}: first difference at {} ns ({runs} runs against {own_runs}):\n{}",
                a.at,
                differences(a, b)
            );
        }
    }
}
