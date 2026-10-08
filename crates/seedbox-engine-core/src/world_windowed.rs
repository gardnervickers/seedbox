//! Experimental windowed scheduler over the shared world: rounds planned by
//! [`crate::window`], machines executed serially or on a work-stealing pool.
//! See docs/windowed-scheduler.md. Static worlds only: no control, faults,
//! triggers, scenarios, forks or checkpoints.

use crate::links;
use crate::window as sw;
use crate::world::{Payload, SliceRequest, State, Stop, World, WorldMachine};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// A frame on the wire. The sender is the event key's source.
type Frame = Vec<u8>;

/// Performance settings. None of them changes a run's results.
#[derive(Clone, Debug, Default)]
pub struct Workers {
    /// Machine threads; 1 runs every machine on the calling thread.
    pub count: usize,
    /// Host time to waste before each run of machine `i`, times `n - i`
    /// (testing only): later machines finish first.
    pub skew: Duration,
}

/// One machine as the window executor sees it.
struct Guest<'a, M> {
    m: &'a mut M,
    skew: Duration,
    /// Receiver outcomes: (nominal time, sender, accepted).
    received: Vec<(u64, usize, bool)>,
    /// Management readiness reported during the run.
    ready: Vec<u64>,
}

impl<M: WorldMachine> Guest<'_, M> {
    /// The machine's own next action, ignoring queued events.
    fn next(&self) -> u64 {
        match self.m.state {
            State::Exited(_) => sw::NEVER,
            State::Running => self.m.time,
            State::Halted if self.m.has_pending_irq() => self.m.time,
            State::Halted => self.m.next_deadline().max(self.m.time),
        }
    }

    fn drain(&mut self) -> Result<(), String> {
        let name = self.m.name.clone();
        if !self.m.control_requests.is_empty() {
            return Err(format!(
                "{name}: runtime control request under the windowed scheduler"
            ));
        }
        if self.m.committed_console.take().is_some() {
            return Err(format!(
                "{name}: console trigger under the windowed scheduler"
            ));
        }
        for (id, at, success) in std::mem::take(&mut self.m.fault_effects) {
            if id == 0 && success {
                self.ready.push(at);
            } else {
                return Err(format!(
                    "{name}: fault effect {id} under the windowed scheduler"
                ));
            }
        }
        Ok(())
    }
}

impl<M: WorldMachine> sw::Machine<Frame> for Guest<'_, M> {
    type Context = M::Clock;

    fn run(
        &mut self,
        clock: &mut M::Clock,
        a: &sw::Assignment<Frame>,
    ) -> Result<sw::Report<Frame>, String> {
        std::thread::sleep(self.skew);
        let slice_end = a.slice_end();
        let mut outputs = Vec::new();
        let mut frames = Vec::new();
        let mut i = 0;
        let mut exited = None;
        let mut stalled = None;
        loop {
            if matches!(self.m.state, State::Exited(_)) {
                exited = Some(self.m.time);
                break;
            }
            if self.m.state == State::Halted {
                // Wake at the machine's own next action or the next event's
                // nominal time, as the reference scheduler's key does. An
                // idle machine that cannot act before the slice end stays.
                let event = a.incoming.get(i).map_or(sw::NEVER, |e| e.key.at);
                let wake = self.next().min(event);
                if wake >= slice_end {
                    break;
                }
                self.m.wake(wake)?;
            }
            if self.m.time >= slice_end {
                break;
            }
            while let Some(e) = a
                .incoming
                .get(i)
                .filter(|e| e.key.at <= self.m.time.saturating_add(a.delivery_early))
            {
                let sw::Source::Machine(from) = e.key.source else {
                    return Err("world events are not supported".into());
                };
                let accepted = self.m.deliver_frame(&e.payload)?;
                self.received.push((e.key.at, from, accepted));
                i += 1;
            }
            let event = a.incoming.get(i).map_or(sw::NEVER, |e| e.key.at);
            let limit = a.stop.min(event);
            let early = if limit == a.stop {
                a.stop_early
            } else {
                a.delivery_early
            };
            let before = (self.m.time, i, outputs.len());
            self.m.observe_console = false;
            let request = SliceRequest {
                hard_limit: limit,
                early_slack: early,
                observe_console: false,
            };
            let stop = self.m.advance(clock, request, &mut frames)?;
            self.drain()?;
            for f in frames.drain(..) {
                if outputs.len() == a.max_outputs {
                    return Err(format!(
                        "{}: more than {} frames in one round",
                        self.m.name, a.max_outputs
                    ));
                }
                outputs.push(sw::Output {
                    sent_at: self.m.time,
                    payload: f,
                });
            }
            match stop {
                Stop::Exited => {
                    exited = Some(self.m.time);
                    break;
                }
                Stop::Barrier | Stop::Control => {
                    return Err(format!(
                        "{}: {stop:?} under the windowed scheduler",
                        self.m.name
                    ))
                }
                Stop::Reached | Stop::Halted | Stop::Sent => {}
            }
            // Equal-time returns are legal (a send, an early event), but the
            // identical stalled state twice is a scheduler bug.
            if (self.m.time, i, outputs.len()) == before
                && matches!(stop, Stop::Reached | Stop::Halted)
            {
                if stalled == Some(before) {
                    return Err(format!(
                        "{}: no progress at {} toward {limit}",
                        self.m.name, self.m.time
                    ));
                }
                stalled = Some(before);
            } else {
                stalled = None;
            }
        }
        Ok(sw::Report {
            time: self.m.time,
            next: if exited.is_some() {
                sw::NEVER
            } else {
                self.next()
            },
            consumed: i,
            outputs,
            exited,
        })
    }
}

/// Link policy and its randomness, applied by the coordinator in commit
/// order. Static worlds only: nothing is ever paused, down or cut.
struct Net<'a> {
    faults: &'a mut crate::faults::Faults,
    macs: &'a [[u8; 6]],
    dropped: &'a mut u64,
    link_counts: &'a mut std::collections::BTreeMap<(usize, usize), [u64; 4]>,
    semantic: &'a mut crate::network_counters::NetworkCounters,
}

impl Net<'_> {
    fn count(&mut self, at: u64, from: usize, to: usize, outcome: usize) {
        self.link_counts.entry((from, to)).or_default()[outcome] += 1;
        self.semantic.record(at, from, to, outcome);
    }
}

impl sw::Router<Frame> for Net<'_> {
    fn route(
        &mut self,
        src: usize,
        sent_at: u64,
        frame: Frame,
        out: &mut Vec<sw::Delivery<Frame>>,
    ) {
        if frame.len() < 14 {
            return;
        }
        let dst_mac: [u8; 6] = frame[0..6].try_into().expect("a 14-byte header");
        for dst in links::candidates(src, dst_mac, self.macs) {
            match self.faults.route(src, dst, sent_at, &frame, false) {
                links::Outcome::Cut => {
                    *self.dropped += 1;
                    self.count(sent_at, src, dst, 1);
                }
                links::Outcome::Lost => {
                    *self.dropped += 1;
                    self.count(sent_at, src, dst, 3);
                }
                links::Outcome::Deliver(at) => out.push(sw::Delivery {
                    to: dst,
                    at,
                    payload: frame.clone(),
                }),
            }
        }
    }
}

/// Reject what the windowed scheduler does not support, before booting.
pub fn check_supported(cfg: &Value, forks_or_checkpoints: bool) -> Result<(), String> {
    let unsupported = |what: &str| Err(format!("--scheduler windowed does not support {what}"));
    if !cfg["control"].is_null() {
        return unsupported("runtime control");
    }
    if cfg["faults"].as_array().is_some_and(|f| !f.is_empty()) {
        return unsupported("faults");
    }
    if cfg["scenario"].as_array().is_some_and(|s| !s.is_empty()) {
        return unsupported("scenario events");
    }
    if cfg["kill_restart"].is_object() {
        return unsupported("kill_restart");
    }
    if cfg["boot"].is_object() {
        return unsupported("boot forks");
    }
    if !cfg["checkpoint"].is_null() || forks_or_checkpoints {
        return unsupported("checkpoints or forks");
    }
    Ok(())
}

/// Run a static world to its end with every machine on the calling thread,
/// using the world's clock. Returns the outcome word and the scheduler's
/// report section; undelivered frames are left in `world.events`.
pub fn simulate<M: WorldMachine>(
    world: &mut World<M>,
    duration: u64,
    end_on: &Option<String>,
    skew: Duration,
    boundary: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(&'static str, Value), String> {
    windowed(
        world,
        duration,
        end_on,
        skew,
        1,
        |plan, guests, clock, net| sw::run_serial(plan, guests, clock, net, boundary),
    )
}

/// As [`simulate`], with machines on a work-stealing pool of `workers`
/// threads. Each thread opens its own clock with `make_clock`.
pub fn simulate_parallel<M, F>(
    world: &mut World<M>,
    duration: u64,
    end_on: &Option<String>,
    workers: Workers,
    make_clock: F,
    boundary: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(&'static str, Value), String>
where
    M: WorldMachine + Send,
    F: Fn(usize) -> Result<M::Clock, String> + Sync,
{
    let count = workers.count;
    windowed(
        world,
        duration,
        end_on,
        workers.skew,
        count,
        |plan, guests, _, net| sw::run_parallel(plan, guests, count, make_clock, net, boundary),
    )
}

/// Outputs one machine may buffer in one round.
const MAX_OUTPUTS: usize = 1 << 16;

fn windowed<M, X>(
    world: &mut World<M>,
    duration: u64,
    end_on: &Option<String>,
    skew: Duration,
    workers: usize,
    execute: X,
) -> Result<(&'static str, Value), String>
where
    M: WorldMachine,
    X: FnOnce(
        &mut sw::World<Frame>,
        &mut [Guest<'_, M>],
        &mut M::Clock,
        &mut Net<'_>,
    ) -> Result<(sw::Outcome, sw::Stats), sw::Error>,
{
    world.check_active()?;
    let n = world.machines.len();
    let end_on = match end_on {
        Some(name) => Some(
            world
                .machines
                .iter()
                .position(|m| &m.name == name)
                .ok_or(format!("end_on_exit: unknown machine {name}"))?,
        ),
        None => None,
    };
    let params = sw::Params {
        latency: world.lookahead,
        early: world.early,
        duration,
        end_on,
        max_outputs: MAX_OUTPUTS,
    };
    let mut plan = sw::World::<Frame>::new(n, params).map_err(|e| e.to_string())?;
    let World {
        machines,
        clock,
        faults,
        macs,
        dropped,
        link_counts,
        semantic_counts,
        ..
    } = world;
    let mut guests: Vec<Guest<M>> = machines
        .iter_mut()
        .enumerate()
        .map(|(i, m)| Guest {
            m,
            skew: skew * (n - i) as u32,
            received: Vec::new(),
            ready: Vec::new(),
        })
        .collect();
    let mut net = Net {
        faults,
        macs,
        dropped,
        link_counts,
        semantic: semantic_counts,
    };
    let started = Instant::now();
    let (outcome, stats) =
        execute(&mut plan, &mut guests, clock, &mut net).map_err(|e| e.to_string())?;
    let host_seconds = started.elapsed().as_secs_f64();

    // Receiver outcomes and readiness, in a fixed order.
    let mut received = Vec::new();
    let mut ready = Vec::new();
    for (to, g) in guests.iter_mut().enumerate() {
        received.extend(
            g.received
                .drain(..)
                .map(|(at, from, ok)| (at, from, to, ok)),
        );
        ready.extend(g.ready.drain(..).map(|at| (at, to)));
    }
    drop(guests);
    received.sort();
    for (at, from, to, accepted) in received {
        if accepted {
            world.delivered += 1;
            world.count_link(at, from, to, 0);
        } else {
            world.dropped += 1;
            world.count_link(at, from, to, 2);
        }
    }
    ready.sort();
    for (at, m) in ready {
        if let Err(error) = world.faults.management_ready(m, at, world.latency) {
            world.faults.error = Some(error);
        }
    }
    // Leave undelivered frames where the report looks for them.
    for (to, key, bytes) in plan.pending() {
        let sw::Source::Machine(from) = key.source else {
            continue;
        };
        world.seq += 1;
        world.events[to].insert(
            (key.at, world.seq),
            Payload::Frame {
                from,
                bytes: bytes.clone(),
            },
        );
    }
    let committed = plan.committed();
    world.semantic_counts.advance(committed);
    world.slices = stats.rounds;
    let (word, exit) = match outcome {
        sw::Outcome::Duration { .. } => ("duration", None),
        sw::Outcome::Quiescent { .. } => ("quiescent", None),
        sw::Outcome::Exited {
            machine, exit_at, ..
        } => (
            "end_machine_exited",
            Some(json!({"machine": world.machines[machine].name, "exit_ns": exit_at})),
        ),
    };
    let rounds = plan.rounds();
    let evidence = json!({
        "name": "windowed",
        "machine_workers": workers.max(1),
        "committed_ns": committed,
        "end_machine_exit": exit,
        "rounds": stats.rounds,
        "exact_rounds": rounds.iter().filter(|r| r.exact).count(),
        "machine_runs": stats.machine_runs,
        "migrations": stats.migrations,
        "effects_digest": format!("{:016x}", plan.effects_digest()),
        "window_ns": {
            "min": stats.window_ns_min,
            "max": stats.window_ns_max,
            "mean": if stats.rounds == 0 { 0 } else { (stats.window_ns_sum / stats.rounds as u128) as u64 },
        },
        "host": {
            "seconds": host_seconds,
            "coordinator_s": stats.coordinator.as_secs_f64(),
            "machine_s": stats.machine.as_secs_f64(),
            "round_wall_s": stats.round_wall.as_secs_f64(),
        },
    });
    Ok((word, evidence))
}
