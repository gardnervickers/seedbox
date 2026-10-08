use super::*;
use std::collections::BTreeMap;
#[derive(Default)]
struct Counts {
    probes: u64,
    false_alarms: u64,
    clock: u64,
    mismatches: BTreeMap<String, u64>,
}
struct Machine {
    states: Vec<(u64, u64)>,
    at: usize,
    marker: u64,
    changing: bool,
    plan: LandingPlan,
    clears: usize,
    counts: Counts,
}
impl Machine {
    fn new(states: Vec<(u64, u64)>) -> Self {
        Self {
            states,
            at: 0,
            marker: 100,
            changing: false,
            plan: LandingPlan::Exact,
            clears: 0,
            counts: Counts::default(),
        }
    }
    fn state(&self) -> (u64, u64) {
        let (key, ram) = self.states[self.at % self.states.len()];
        (key, if self.changing { self.at as u64 } else { ram })
    }
    fn advance(&mut self) {
        self.at += 1;
    }
}
impl ProgressContext for Machine {
    fn name(&self) -> &str {
        "scripted"
    }
    fn accounting(&mut self) -> Accounting<'_> {
        Accounting {
            probes: &mut self.counts.probes,
            false_alarms: &mut self.counts.false_alarms,
            clock_progress: &mut self.counts.clock,
            mismatches: &mut self.counts.mismatches,
        }
    }
}
impl ProbeMachine for Machine {
    type Key = u64;
    type Saved = (u64, u64);
    const PC_NAME: &'static str = "pc";
    fn pc(key: &u64) -> u64 {
        *key
    }
    fn key(&mut self) -> Result<u64, String> {
        Ok(self.state().0)
    }
    fn clock_marker(&self) -> u64 {
        self.marker
    }
    fn save(&mut self) -> Result<Self::Saved, String> {
        Ok(self.state())
    }
    fn compare(&mut self, saved: &mut Self::Saved) -> Result<Comparison, String> {
        Ok(if *saved == self.state() {
            Comparison::Equal
        } else {
            Comparison::Different("ram".into())
        })
    }
    fn arm_next_landing(&mut self) -> Result<LandingPlan, String> {
        Ok(self.plan)
    }
    fn clear_debugger(&mut self) -> Result<(), String> {
        self.clears += 1;
        Ok(())
    }
}
type Progress = PvProgress<u64, (u64, u64)>;
fn begin(progress: &mut Progress, m: &mut Machine) {
    assert!(!progress.judge(Exit::Watchdog, m).unwrap());
    assert!(!progress.hold_watchdog());
    assert!(!progress.judge(Exit::Watchdog, m).unwrap());
    assert!(progress.hold_watchdog());
}
fn run(progress: &mut Progress, m: &mut Machine) -> Result<Decision, String> {
    for _ in 0..PROBE_STEPS + 10 {
        let decision = progress.prepare(m, u64::MAX)?;
        if decision != Decision::Step {
            return Ok(decision);
        }
        m.advance();
        assert!(!progress.judge(Exit::Landing, m)?);
    }
    panic!("proof exceeded bounded search");
}
#[test]
fn a_true_cycle_lands_at_the_same_canonical_state_from_each_start() {
    for at in 0..12 {
        let mut m = Machine::new(vec![(30, 7), (10, 8), (20, 9)]);
        m.at = at;
        let mut p = Progress::new();
        begin(&mut p, &mut m);
        assert_eq!(run(&mut p, &mut m).unwrap(), Decision::Idle);
        assert_eq!(m.state(), (10, 8));
        assert!(!p.hold_watchdog());
        assert_eq!(m.counts.false_alarms, 0);
    }
}
#[test]
fn equal_registers_do_not_prove_a_cycle_when_ram_changes() {
    let mut m = Machine::new(vec![(1, 0)]);
    m.changing = true;
    let mut p = Progress::new();
    begin(&mut p, &mut m);
    assert_eq!(run(&mut p, &mut m).unwrap(), Decision::Resume);
    assert_eq!(m.counts.false_alarms, 1);
    assert!(m.counts.mismatches["ram"] > 0);
    assert!(!p.hold_watchdog());
    assert_eq!(m.clears, 1);
}
#[test]
fn repeated_minimal_keys_cannot_choose_an_ambiguous_canonical_state() {
    let mut m = Machine::new(vec![(2, 0), (1, 0), (1, 1)]);
    let mut p = Progress::new();
    begin(&mut p, &mut m);
    assert!(run(&mut p, &mut m)
        .unwrap_err()
        .contains("no unique canonical point"));
}
#[test]
fn clock_progress_abandons_even_after_search_reference_replacement() {
    for marker in [99, 101] {
        let mut m = Machine::new(vec![(1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]);
        let mut p = Progress::new();
        begin(&mut p, &mut m);
        for _ in 0..3 {
            assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Step);
            m.advance();
            p.judge(Exit::Landing, &mut m).unwrap();
        }
        assert_eq!(p.probe.as_ref().unwrap().last, Some(100));
        m.marker = marker;
        assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Resume);
        assert_eq!((m.counts.false_alarms, m.counts.clock), (1, 1));
        assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Resume);
        assert_eq!(m.counts.false_alarms, 1);
    }
}
#[test]
fn planner_cap_and_lost_landing_resume_without_proving_idle() {
    for cap in [false, true] {
        let mut m = Machine::new(vec![(1, 0)]);
        if cap {
            m.plan = LandingPlan::PlannerCap;
        }
        let mut p = Progress::new();
        begin(&mut p, &mut m);
        if !cap {
            assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Step);
        }
        assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Resume);
        assert_eq!(m.counts.false_alarms, 1);
        assert_eq!(m.clears, 1);
        assert!(!p.hold_watchdog());
    }
}
#[test]
fn unsupported_landing_and_out_of_probe_debug_are_errors() {
    let mut m = Machine::new(vec![(1, 0)]);
    m.plan = LandingPlan::Unsupported;
    let mut p = Progress::new();
    assert!(p.judge(Exit::Landing, &mut m).is_err());
    begin(&mut p, &mut m);
    assert!(p
        .prepare(&mut m, u64::MAX)
        .unwrap_err()
        .contains("unsupported"));
}
#[test]
fn natural_exit_resets_watchdog_backoff_and_abandons_active_proof() {
    let mut m = Machine::new(vec![(1, 0)]);
    m.plan = LandingPlan::PlannerCap;
    let mut p = Progress::new();
    begin(&mut p, &mut m);
    assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Resume);
    p.judge(Exit::Watchdog, &mut m).unwrap();
    assert!(!p.hold_watchdog());
    p.judge(Exit::Watchdog, &mut m).unwrap();
    assert!(p.hold_watchdog());
    assert!(p.judge(Exit::Natural, &mut m).unwrap());
    assert_eq!(m.counts.false_alarms, 2);
    assert!(!p.hold_watchdog());
    begin(&mut p, &mut m);
}
#[test]
fn lost_canonical_cycle_and_false_alarm_limit_are_errors() {
    let mut m = Machine::new(vec![(1, 0)]);
    let mut p = Progress::new();
    p.probe = Some(Probe {
        last: Some(100),
        phase: Phase::Canon { target: 2, left: 0 },
    });
    p.landed = true;
    assert!(p
        .prepare(&mut m, u64::MAX)
        .unwrap_err()
        .contains("lost the spin cycle"));
    let mut p = Progress::new();
    m.plan = LandingPlan::PlannerCap;
    begin(&mut p, &mut m);
    assert!(p
        .prepare(&mut m, 1)
        .unwrap_err()
        .contains("1 spin checks found no cycle"));
}

#[test]
fn unrelated_interrupt_does_not_start_a_probe_or_confirm_a_landing() {
    let mut m = Machine::new(vec![(1, 0)]);
    let mut p = Progress::new();
    for _ in 0..10 {
        assert!(!p.judge(Exit::OtherInterruption, &mut m).unwrap());
    }
    assert!(!p.hold_watchdog());
    assert_eq!(m.counts.probes, 0);
    begin(&mut p, &mut m);
    assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Step);
    p.judge(Exit::OtherInterruption, &mut m).unwrap();
    assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Resume);
    assert_eq!(m.counts.false_alarms, 1);
}

#[test]
fn accounting_is_per_machine_and_restored_counts_control_the_limit() {
    let mut p = Progress::new();
    let mut first = Machine::new(vec![(1, 0)]);
    first.plan = LandingPlan::PlannerCap;
    first.counts.false_alarms = 3;
    begin(&mut p, &mut first);
    assert!(p
        .prepare(&mut first, 4)
        .unwrap_err()
        .contains("4 spin checks"));
    let mut second = Machine::new(vec![(2, 0)]);
    second.plan = LandingPlan::PlannerCap;
    p.judge(Exit::Natural, &mut second).unwrap();
    begin(&mut p, &mut second);
    assert_eq!(p.prepare(&mut second, 4).unwrap(), Decision::Resume);
    assert_eq!(
        (first.counts.false_alarms, second.counts.false_alarms),
        (4, 1)
    );
    second.counts.false_alarms = 3; // Restore the accounting of an earlier machine snapshot.
    p.judge(Exit::Natural, &mut second).unwrap();
    begin(&mut p, &mut second);
    assert!(p
        .prepare(&mut second, 4)
        .unwrap_err()
        .contains("4 spin checks"));
}
#[test]
fn repeated_failed_probes_back_off_to_the_cap_and_fresh_probes_rebase_clock() {
    let mut p = Progress::new();
    let mut m = Machine::new(vec![(1, 0)]);
    m.plan = LandingPlan::PlannerCap;
    begin(&mut p, &mut m);
    for attempt in 0..13 {
        assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Resume);
        let skip = (1u32 << attempt).min(1024);
        assert_eq!(p.skip, skip);
        for _ in 0..skip {
            p.judge(Exit::Watchdog, &mut m).unwrap();
            assert!(!p.hold_watchdog());
        }
        p.judge(Exit::Watchdog, &mut m).unwrap();
        assert!(p.hold_watchdog());
    }
    p.judge(Exit::Natural, &mut m).unwrap();
    m.marker = 500;
    m.plan = LandingPlan::Exact;
    begin(&mut p, &mut m);
    assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Step);
    assert_eq!(p.probe.as_ref().unwrap().last, Some(500));
    m.marker = 501;
    p.judge(Exit::Landing, &mut m).unwrap();
    assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Resume);
    p.judge(Exit::Natural, &mut m).unwrap();
    m.marker = 100;
    begin(&mut p, &mut m);
    assert_eq!(p.prepare(&mut m, u64::MAX).unwrap(), Decision::Step);
    assert_eq!(p.probe.as_ref().unwrap().last, Some(100));
}
