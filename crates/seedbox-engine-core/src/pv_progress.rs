//! Watchdog admission and deterministic cycle proof, independent of CPU/host APIs.
//! Host timers only request observation; elapsed host time never proves guest idle.

pub const PROBE_STEPS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Natural,
    Watchdog,
    Landing,
    OtherInterruption,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Resume,
    Step,
    Idle,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandingPlan {
    Exact,
    PlannerCap,
    Unsupported,
}
pub enum Comparison {
    Equal,
    Different(String),
}
enum Observation {
    Probe,
    FalseAlarm,
    ClockProgress,
    Mismatch(String),
}

/// Per-machine accounting remains with the machine, including across snapshot
/// restore, even when one progress engine is reused by a world. The shared
/// policy alone increments these counters. They must not be filtered or reset
/// as a diagnostic sink: the false-alarm limit reads this same accounting.
pub struct Accounting<'a> {
    pub probes: &'a mut u64,
    pub false_alarms: &'a mut u64,
    pub clock_progress: &'a mut u64,
    pub mismatches: &'a mut std::collections::BTreeMap<String, u64>,
}
impl Accounting<'_> {
    fn observe(&mut self, observation: Observation) {
        match observation {
            Observation::Probe => *self.probes += 1,
            Observation::FalseAlarm => *self.false_alarms += 1,
            Observation::ClockProgress => *self.clock_progress += 1,
            Observation::Mismatch(part) => *self.mismatches.entry(part).or_default() += 1,
        }
    }
}
pub trait ProgressContext {
    fn name(&self) -> &str;
    fn accounting(&mut self) -> Accounting<'_>;
}

/// Stopped architecture primitives. Equal keys are only a comparison prefilter.
/// A baseline comparison must cover CPU, PV page and RAM, including device DMA.
/// Unsupported dirty tracking requires byte comparison of the full RAM baseline.
/// A baseline lives until replaced or the active probe ends; do not switch
/// machines while a probe is active. Normalize only debugger-owned CPU bits.
/// Exact landings must be confirmed by the architecture before reporting Landing.
/// Arming/comparing/clearing must not advance guest time, inject guest IRQs, retire
/// pending I/O, or add host/debug events to the semantic witness.
pub trait ProbeMachine: ProgressContext {
    type Key: Clone + Ord;
    type Saved;
    const PC_NAME: &'static str;
    fn pc(key: &Self::Key) -> u64;
    fn key(&mut self) -> Result<Self::Key, String>;
    fn clock_marker(&self) -> u64;
    fn save(&mut self) -> Result<Self::Saved, String>;
    fn compare(&mut self, saved: &mut Self::Saved) -> Result<Comparison, String>;
    fn arm_next_landing(&mut self) -> Result<LandingPlan, String>;
    fn clear_debugger(&mut self) -> Result<(), String>;
}

pub struct PvProgress<K, S> {
    progressed: bool,
    skip: u32,
    backoff: u32,
    probe: Option<Probe<K, S>>,
    landed: bool,
}
struct Probe<K, S> {
    last: Option<u64>,
    phase: Phase<K, S>,
}
enum Phase<K, S> {
    Search {
        saved: Option<Box<S>>,
        keys: Vec<K>,
        power: usize,
        steps: usize,
    },
    Canon {
        target: K,
        left: usize,
    },
}
impl<K, S> Probe<K, S> {
    fn new() -> Self {
        Self {
            last: None,
            phase: Phase::Search {
                saved: None,
                keys: vec![],
                power: 1,
                steps: 0,
            },
        }
    }
}
impl<K: Clone + Ord, S> Default for PvProgress<K, S> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: Clone + Ord, S> PvProgress<K, S> {
    pub fn new() -> Self {
        Self {
            progressed: true,
            skip: 0,
            backoff: 0,
            probe: None,
            landed: false,
        }
    }
    /// Hold host watchdog delivery while a probe landing is armed. A host tick
    /// must not hide a breakpoint at the same instruction.
    pub fn hold_watchdog(&self) -> bool {
        self.probe.is_some()
    }

    /// Returns whether this was a natural, guest-visible execution boundary.
    /// Landing and watchdog stops neither advance virtual time nor enter witnesses.
    pub fn judge(
        &mut self,
        exit: Exit,
        observer: &mut impl ProgressContext,
    ) -> Result<bool, String> {
        match exit {
            Exit::Watchdog => {
                if self.skip > 0 {
                    self.skip -= 1;
                } else if self.probe.is_none() && !std::mem::take(&mut self.progressed) {
                    observer.accounting().observe(Observation::Probe);
                    self.probe = Some(Probe::new());
                    self.landed = true;
                }
                Ok(false)
            }
            Exit::Landing => {
                if self.probe.is_none() {
                    return Err(format!(
                        "{}: debug exit outside a spin check",
                        observer.name()
                    ));
                }
                self.landed = true;
                Ok(false)
            }
            Exit::OtherInterruption => {
                self.landed = false;
                Ok(false)
            }
            Exit::Natural => {
                self.progressed = true;
                self.backoff = 0;
                self.skip = 0;
                if self.probe.take().is_some() {
                    observer.accounting().observe(Observation::FalseAlarm);
                }
                Ok(true)
            }
        }
    }
    /// Prepare a stopped machine. Only a complete recurring state at a unique
    /// canonical landing permits Idle; every incomplete proof resumes normally.
    pub fn prepare<M: ProbeMachine<Key = K, Saved = S>>(
        &mut self,
        machine: &mut M,
        max_false_alarms: u64,
    ) -> Result<Decision, String> {
        if self.probe.is_some() {
            let step = if std::mem::take(&mut self.landed) {
                self.probe_step(machine)?
            } else {
                self.probe = None;
                machine.accounting().observe(Observation::FalseAlarm);
                Decision::Resume
            };
            match step {
                Decision::Idle => {
                    self.probe = None;
                    return Ok(Decision::Idle);
                }
                Decision::Step => return Ok(Decision::Step),
                Decision::Resume => {
                    if *machine.accounting().false_alarms >= max_false_alarms {
                        let count = *machine.accounting().false_alarms;
                        let mismatches = format!("{:?}", machine.accounting().mismatches);
                        return Err(format!(
                            "{}: {} spin checks found no cycle; first differences: {}",
                            machine.name(),
                            count,
                            mismatches
                        ));
                    }
                    self.backoff = (self.backoff * 2).clamp(1, 1 << 10);
                    self.skip = self.backoff;
                }
            }
        }
        machine.clear_debugger()?;
        Ok(Decision::Resume)
    }
    fn probe_step<M: ProbeMachine<Key = K, Saved = S>>(
        &mut self,
        machine: &mut M,
    ) -> Result<Decision, String> {
        let key = machine.key()?;
        let mut probe = self.probe.take().expect("active probe");
        if matches!(probe.phase, Phase::Search { .. }) {
            let last = machine.clock_marker();
            if last != *probe.last.get_or_insert(last) {
                machine.accounting().observe(Observation::FalseAlarm);
                machine.accounting().observe(Observation::ClockProgress);
                return Ok(Decision::Resume);
            }
        }
        let next = match probe.phase {
            Phase::Search {
                mut saved,
                mut keys,
                power,
                steps,
            } => {
                if steps >= PROBE_STEPS {
                    machine.accounting().observe(Observation::FalseAlarm);
                    return Ok(Decision::Resume);
                }
                let steps = steps + 1;
                let back = match saved.as_mut() {
                    Some(saved) if key == keys[0] => match machine.compare(saved)? {
                        Comparison::Equal => true,
                        Comparison::Different(part) => {
                            machine.accounting().observe(Observation::Mismatch(part));
                            false
                        }
                    },
                    _ => false,
                };
                if back {
                    let target = keys.iter().min().expect("saved reference key").clone();
                    if keys.iter().filter(|k| **k == target).count() > 1 {
                        return Err(format!("{}: spin cycle at {} {:#x} has no unique canonical point (equal registers at two points of it)", machine.name(), M::PC_NAME, M::pc(&target)));
                    }
                    if key == target {
                        return Ok(Decision::Idle);
                    }
                    Phase::Canon {
                        target,
                        left: keys.len(),
                    }
                } else if saved.is_none() || keys.len() >= power {
                    let power = if saved.is_none() { 1 } else { power * 2 };
                    Phase::Search {
                        saved: Some(Box::new(machine.save()?)),
                        keys: vec![key],
                        power,
                        steps,
                    }
                } else {
                    keys.push(key);
                    Phase::Search {
                        saved,
                        keys,
                        power,
                        steps,
                    }
                }
            }
            Phase::Canon { target, left } => {
                if key == target {
                    return Ok(Decision::Idle);
                }
                if left == 0 {
                    return Err(format!(
                        "{}: lost the spin cycle at {} {:#x}",
                        machine.name(),
                        M::PC_NAME,
                        M::pc(&target)
                    ));
                }
                Phase::Canon {
                    target,
                    left: left - 1,
                }
            }
        };
        match machine.arm_next_landing()? {
            LandingPlan::Exact => (),
            LandingPlan::PlannerCap => {
                machine.accounting().observe(Observation::FalseAlarm);
                return Ok(Decision::Resume);
            }
            LandingPlan::Unsupported => {
                return Err(format!(
                    "{}: cycle-proof landing is unsupported",
                    machine.name()
                ))
            }
        }
        probe.phase = next;
        self.probe = Some(probe);
        Ok(Decision::Step)
    }
}

#[cfg(test)]
#[path = "pv_progress_tests.rs"]
mod tests;
