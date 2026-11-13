use super::*;
use crate::random::Rng;

/// Simulated time that sleeps advance, and every action applied; actions
/// named in `refuse` are refused. Each clock read and action also takes
/// `call_ns`, as the guest control calls do while the world runs on.
#[derive(Default)]
struct Log {
    now_ns: u64,
    call_ns: u64,
    applied: Vec<(u64, Action)>,
    refuse: Vec<Action>,
}

impl World for Log {
    fn now_ns(&mut self) -> Result<u64, Error> {
        self.now_ns += self.call_ns;
        Ok(self.now_ns)
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        self.now_ns += duration.as_nanos() as u64;
        Ok(())
    }
    fn apply(&mut self, action: &Action) -> Result<(), Error> {
        self.now_ns += self.call_ns;
        if self.refuse.contains(action) {
            return Err(Error("refused".into()));
        }
        self.applied.push((self.now_ns, action.clone()));
        Ok(())
    }
}

fn nodes() -> Vec<String> {
    (1..=5).map(|i| format!("n{i}")).collect()
}

const SECOND: Duration = Duration::from_secs(1);

fn phases(hold: u64) -> Phases {
    Phases {
        warm_up: SECOND,
        hold: Duration::from_secs(hold),
        tail: 2 * SECOND,
    }
}

#[test]
fn subsets_and_splits_have_jepsens_sizes() {
    for (subset, sizes) in [
        (Subset::One, [1, 1, 1]),
        (Subset::Minority, [1, 1, 2]),
        (Subset::Majority, [2, 2, 3]),
        (Subset::All, [2, 3, 5]),
    ] {
        assert_eq!([2, 3, 5].map(|n| subset.count(n)), sizes, "{subset:?}");
    }
    let mut rng = Rng::new(3);
    for split in [Split::Minority, Split::Halves, Split::Isolate] {
        let (left, right) = split.sides(&mut rng, &nodes());
        assert!(!left.is_empty() && !right.is_empty(), "{split:?}");
        assert_eq!(left.len() + right.len(), 5);
        assert!(left.windows(2).all(|w| w[0] < w[1]));
        assert!(left.iter().all(|n| !right.contains(n)));
    }
}

#[test]
fn kill_rounds_stop_and_restart_the_same_victims_within_the_hold() {
    let mut world = Log::default();
    let mut nemesis = Schedule::new(
        phases(10),
        KillRestart {
            nodes: nodes(),
            down: (SECOND, 2 * SECOND),
            up: (SECOND, SECOND),
        },
    );
    nemesis.run(&mut world, &mut Rng::new(9)).unwrap();
    assert!(nemesis.rounds >= 3);
    // Faults start after the warm-up; the tail follows the last round.
    assert!(world.applied[0].0 >= 1_000_000_000);
    let (stops, restarts): (Vec<_>, Vec<_>) = world
        .applied
        .iter()
        .partition(|(_, action)| matches!(action, Action::Stop { .. }));
    assert_eq!(stops.len(), restarts.len());
    assert!(restarts
        .iter()
        .all(|(_, action)| matches!(action, Action::Restart { .. })));
    // Every round undid itself, so the final heal does nothing.
    let applied = world.applied.len();
    nemesis.heal(&mut world).unwrap();
    assert_eq!(world.applied.len(), applied);
    // The same seed makes the same schedule.
    let mut again = Log::default();
    let mut replay = Schedule::new(
        phases(10),
        KillRestart {
            nodes: nodes(),
            down: (SECOND, 2 * SECOND),
            up: (SECOND, SECOND),
        },
    );
    replay.run(&mut again, &mut Rng::new(9)).unwrap();
    assert_eq!(again.applied, world.applied);
}

#[test]
fn partitions_name_their_handles_and_heal_them() {
    let mut world = Log::default();
    let mut nemesis = Schedule::new(
        phases(4),
        Partitions {
            nodes: nodes(),
            split: Split::Minority,
            length: (SECOND, SECOND),
            gap: (SECOND, SECOND),
        },
    );
    nemesis.run(&mut world, &mut Rng::new(1)).unwrap();
    let handles: Vec<_> = world
        .applied
        .iter()
        .map(|(_, action)| match action {
            Action::Partition { handle, .. } => format!("+{handle}"),
            Action::Heal { handle } => format!("-{handle}"),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        handles,
        [
            "+partition-1",
            "-partition-1",
            "+partition-2",
            "-partition-2"
        ]
    );
}

#[test]
fn heal_undoes_whatever_a_failed_round_left_in_force() {
    let mut world = Log {
        refuse: vec![Action::ResumeProcess {
            machine: "n1".into(),
        }],
        ..Default::default()
    };
    let mut faults = InForce::default();
    faults
        .apply(
            &mut world,
            &Action::PauseProcess {
                machine: "n1".into(),
            },
        )
        .unwrap();
    let handle = faults.name("partition");
    faults
        .apply(
            &mut world,
            &Action::Partition {
                handle: handle.clone(),
                left: vec!["n1".into()],
                right: vec!["n2".into()],
            },
        )
        .unwrap();
    faults
        .apply(
            &mut world,
            &Action::Crash {
                machine: "n2".into(),
            },
        )
        .unwrap();
    // The refused resume is reported, after every other undo was tried.
    let error = faults.heal(&mut world).unwrap_err();
    assert_eq!(error.0, "resume_process: refused");
    let undone: Vec<_> = world.applied[3..].iter().map(|(_, a)| a.clone()).collect();
    assert_eq!(
        undone,
        [
            Action::Heal { handle },
            Action::Reboot {
                machine: "n2".into()
            },
        ]
    );
    assert!(!faults.is_empty());
    world.refuse.clear();
    faults.heal(&mut world).unwrap();
    assert!(faults.is_empty());
}

#[test]
fn a_refused_crash_still_reboots_what_the_round_crashed() {
    let mut world = Log {
        refuse: vec![Action::Crash {
            machine: "n3".into(),
        }],
        ..Default::default()
    };
    let mut nemesis = Schedule::new(
        phases(1),
        CrashAll {
            nodes: nodes(),
            down: SECOND,
            up: SECOND,
        },
    );
    let error = nemesis.run(&mut world, &mut Rng::new(1)).unwrap_err();
    assert!(error.0.starts_with("crash n3"), "{error}");
    let names: Vec<_> = world
        .applied
        .iter()
        .map(|(_, a)| format!("{} {}", a.name(), a.machine().unwrap()))
        .collect();
    assert_eq!(names, ["crash n1", "crash n2", "reboot n1", "reboot n2"]);
    nemesis.heal(&mut world).unwrap();
    assert_eq!(world.applied.len(), 4);
}

#[test]
fn quiet_and_pauses_run_for_their_phases() {
    let mut world = Log::default();
    let mut quiet = Schedule::new(phases(3), Quiet);
    quiet.run(&mut world, &mut Rng::new(1)).unwrap();
    assert!(world.applied.is_empty());
    assert_eq!(quiet.rounds, 3);
    assert_eq!(world.now_ns, 6_000_000_000);
    let mut pauses = Schedule::new(
        phases(2),
        ProcessPauses {
            nodes: nodes(),
            length: SECOND,
            gap: SECOND,
        },
    );
    pauses.run(&mut world, &mut Rng::new(1)).unwrap();
    assert!(matches!(world.applied[0].1, Action::PauseProcess { .. }));
    assert!(matches!(world.applied[1].1, Action::ResumeProcess { .. }));
}

#[test]
fn too_few_nodes_are_an_error_not_a_panic() {
    let mut world = Log::default();
    let one = vec!["n1".to_string()];
    let mut partitions = Schedule::new(
        phases(1),
        Partitions {
            nodes: one.clone(),
            split: Split::Isolate,
            length: (SECOND, SECOND),
            gap: (SECOND, SECOND),
        },
    );
    let error = partitions.run(&mut world, &mut Rng::new(1)).unwrap_err();
    assert_eq!(error.0, "partitions needs at least 2 nodes, has 1");
    let mut pauses = Schedule::new(
        phases(1),
        ProcessPauses {
            nodes: vec![],
            length: SECOND,
            gap: SECOND,
        },
    );
    assert!(pauses.run(&mut world, &mut Rng::new(1)).is_err());
    assert!(world.applied.is_empty());
}

/// A world whose every call takes a microsecond.
fn ticking() -> Log {
    Log {
        call_ns: 1_000,
        ..Default::default()
    }
}

fn crash_all() -> CrashAll {
    CrashAll {
        nodes: nodes(),
        down: SECOND,
        up: SECOND,
    }
}

/// A schedule that holds for `hold` nanoseconds after a one-second warm-up.
fn short(hold: u64) -> Phases {
    Phases {
        warm_up: SECOND,
        hold: Duration::from_nanos(hold),
        tail: SECOND,
    }
}

#[test]
fn a_hold_shorter_than_a_clock_read_still_runs_one_round() {
    for hold in [0, 500] {
        let mut world = ticking();
        let mut schedule = Schedule::new(short(hold), crash_all());
        schedule.run(&mut world, &mut Rng::new(1)).unwrap();
        assert_eq!(schedule.rounds, 1, "hold {hold} ns");
        let names: Vec<_> = world.applied.iter().map(|(_, a)| a.name()).collect();
        assert_eq!(names, [["crash"; 5], ["reboot"; 5]].concat());
        assert!(world.applied[0].0 >= 1_000_000_000, "after the warm-up");
        // The round healed itself: nothing is left in force.
        schedule.heal(&mut world).unwrap();
        assert_eq!(world.applied.len(), 10);
    }
}

/// Installs a partition and stops `n1`, then crashes `n2`.
struct Partial;

impl Round for Partial {
    fn round(
        &mut self,
        faults: &mut InForce,
        world: &mut dyn World,
        pace: &mut dyn Pace,
        _: &mut dyn Random,
    ) -> Result<(), Error> {
        let handle = faults.name("partition");
        faults.apply(
            world,
            &Action::Partition {
                handle,
                left: vec!["n1".into()],
                right: vec!["n2".into()],
            },
        )?;
        faults.apply(
            world,
            &Action::Stop {
                machine: "n1".into(),
            },
        )?;
        pace.wait(world, SECOND)?;
        faults.apply(
            world,
            &Action::Crash {
                machine: "n2".into(),
            },
        )?;
        pace.wait(world, SECOND)
    }
}

#[test]
fn heal_undoes_what_a_failed_round_started_and_reports_heal_failures() {
    let mut world = ticking();
    world.refuse = vec![
        Action::Crash {
            machine: "n2".into(),
        },
        Action::Heal {
            handle: "partition-1".into(),
        },
    ];
    let mut schedule = Schedule::new(short(0), Partial);
    let error = schedule.run(&mut world, &mut Rng::new(1)).unwrap_err();
    assert_eq!(error.0, "refused");
    assert_eq!(world.applied.len(), 2);
    // The refused heal is reported after the restart was still tried.
    let error = schedule.heal(&mut world).unwrap_err();
    assert_eq!(error.0, "heal: refused");
    assert_eq!(
        world.applied[2].1,
        Action::Restart {
            machine: "n1".into()
        }
    );
    assert_eq!(world.applied.len(), 3);
    // What stayed in force is undone by the next heal.
    world.refuse.clear();
    schedule.heal(&mut world).unwrap();
    assert_eq!(
        world.applied[3].1,
        Action::Heal {
            handle: "partition-1".into()
        }
    );
    schedule.heal(&mut world).unwrap();
    assert_eq!(world.applied.len(), 4);
}
