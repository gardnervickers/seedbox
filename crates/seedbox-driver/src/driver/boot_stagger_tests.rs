//! Staggered initial boots: each initial machine but the oracle first runs at
//! a seeded delay below the spread, the same for the same seed, and nothing
//! else moves.
use super::*;
use crate::rng::{RngStream, SeededRng};
use std::sync::Mutex;

const MS: u64 = 1_000_000;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

/// Records the time of every run; idles with no timer of its own.
struct Idle {
    runs: Arc<Mutex<Vec<u64>>>,
}
impl Machine for Idle {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        _upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = context.clock.now();
        while context.inbox.take().is_some() {}
        self.runs.lock().unwrap().push(now);
        Ok(StopReason::Blocked {
            reached: at(now),
            deadline: None,
        })
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

type Runs = Vec<Arc<Mutex<Vec<u64>>>>;
fn world(seed: u64, machines: usize) -> (Driver<Idle>, Runs) {
    let runs: Runs = (0..machines).map(|_| Arc::default()).collect();
    let mut idle = runs.clone().into_iter().map(|runs| Idle { runs });
    let driver = Driver::new_seeded(
        at(0),
        Duration::from_millis(1),
        seed,
        || Ok(()),
        fixture::definition(0),
        (1..machines).map(fixture::definition).collect(),
        Vec::new(),
        NetworkConfig::default(),
        move |_| Ok(idle.next().expect("one machine per definition")),
    )
    .unwrap();
    (driver, runs)
}
/// The time of each machine's first run, after stepping to `upto`.
fn first_runs(driver: &mut Driver<Idle>, runs: &Runs, upto: u64) -> Vec<u64> {
    while driver.position().at < at(upto) {
        match driver.step(at(upto)).unwrap() {
            StepOutcome::BarrierReached { .. } => {}
            StepOutcome::Blocked { .. } => break,
            outcome => panic!("{outcome:?}"),
        }
    }
    runs.iter()
        .map(|runs| *runs.lock().unwrap().first().expect("machine ran"))
        .collect()
}
fn staggered(seed: u64, machines: usize) -> Vec<u64> {
    let (mut driver, runs) = world(seed, machines);
    driver.stagger_boots(BOOT_SPREAD).unwrap();
    first_runs(&mut driver, &runs, 100 * MS)
}

#[test]
fn initial_machines_boot_at_distinct_seeded_times_below_the_spread_and_the_oracle_at_once() {
    let spread = u64::try_from(BOOT_SPREAD.as_nanos()).unwrap();
    let mut all = BTreeSet::new();
    for seed in 1..=12 {
        let first = staggered(seed, 4);
        assert_eq!(first[0], 0, "seed {seed}: the oracle is not staggered");
        let machines: BTreeSet<_> = first[1..].iter().copied().collect();
        assert_eq!(machines.len(), 3, "seed {seed}: distinct boots {first:?}");
        assert!(
            first[1..].iter().all(|&t| t < spread),
            "seed {seed}: {first:?}"
        );
        all.extend(machines);
        // The same seed boots at the same times.
        assert_eq!(first, staggered(seed, 4));
    }
    // Different seeds choose different times, across the spread.
    assert_eq!(all.len(), 36);
    assert!(all.iter().any(|&t| t < spread / 4) && all.iter().any(|&t| t > spread * 3 / 4));
}

/// The next bytes a stream would give, leaving it where it is.
fn peek(rng: &SeededRng) -> [u8; 48] {
    let mut bytes = [0; 48];
    rng.clone().fill_bytes(&mut bytes);
    bytes
}
fn streams(driver: &Driver<Idle>) -> Vec<[u8; 48]> {
    let SimulationRng::Seeded(simulation) = &driver.execution.rng else {
        panic!("seeded world");
    };
    std::iter::once(peek(simulation))
        .chain(
            driver
                .execution
                .machines
                .iter()
                .map(|entry| peek(entry.rng.as_ref().unwrap())),
        )
        .collect()
}

#[test]
fn delays_come_from_machine_seeds_without_drawing() {
    let spread = u64::try_from(BOOT_SPREAD.as_nanos()).unwrap();
    let (mut driver, runs) = world(7, 3);
    let delays: Vec<u64> = driver
        .execution
        .machines
        .iter()
        .map(|entry| entry.rng.as_ref().unwrap().boot_delay(spread))
        .collect();
    let before = streams(&driver);
    driver.stagger_boots(BOOT_SPREAD).unwrap();
    // No stream moved: not the machines' own, not the simulation's.
    assert_eq!(before, streams(&driver));
    let first = first_runs(&mut driver, &runs, 100 * MS);
    assert_eq!(first[1..], delays[1..]);
}

#[test]
fn a_zero_spread_changes_nothing_and_staggering_is_once_before_execution() {
    let (mut driver, runs) = world(3, 3);
    driver.stagger_boots(Duration::ZERO).unwrap();
    assert_eq!(first_runs(&mut driver, &runs, 10 * MS), [0, 0, 0]);
    assert!(matches!(
        driver.stagger_boots(BOOT_SPREAD),
        Err(Error::Control(_))
    ));
    let (mut driver, _) = world(3, 3);
    driver.stagger_boots(BOOT_SPREAD).unwrap();
    assert!(matches!(
        driver.stagger_boots(BOOT_SPREAD),
        Err(Error::Control(_))
    ));
}

#[test]
fn a_machine_paused_before_its_boot_boots_when_resumed() {
    let (mut driver, runs) = world(5, 2);
    driver.stagger_boots(BOOT_SPREAD).unwrap();
    driver
        .with_barrier(|b| {
            b.schedule(
                at(0),
                fixture::auto_key(),
                Action::Pause(Target::Machine(MachineId(1))),
            )?;
            b.schedule(
                at(70 * MS),
                fixture::auto_key(),
                Action::Resume(Target::Machine(MachineId(1))),
            )
        })
        .unwrap();
    assert_eq!(first_runs(&mut driver, &runs, 100 * MS), [0, 70 * MS]);
}

#[test]
fn staggered_machines_stay_switched_off_until_their_boot() {
    let (mut driver, runs) = world(9, 3);
    driver.stagger_boots(BOOT_SPREAD).unwrap();
    assert!(driver.execution.machines[1..]
        .iter()
        .all(|entry| entry.powered_off));
    let expected = staggered(9, 3);
    // Up to just before the first boot, then onward.
    let early = expected[1..].iter().min().unwrap() - 1;
    driver.step(at(early)).unwrap();
    assert!(driver.execution.machines[1..]
        .iter()
        .all(|entry| entry.powered_off));
    assert_eq!(first_runs(&mut driver, &runs, 100 * MS), expected);
    assert!(driver
        .execution
        .machines
        .iter()
        .all(|entry| !entry.powered_off));
}
