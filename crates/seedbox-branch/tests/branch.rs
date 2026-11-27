//! Runs, branches and rewinds over scripted machines, through the Driver's
//! public API.
use seedbox_branch::{Branch, Entry, Error, Input, Program, Record, Run, Stop};
use seedbox_driver::{
    driver::{
        faults::Engine, Action, Checkpoint, Driver, InputKey, MachineId, MachineState, Target,
        Transition,
    },
    pv::{CommandResult, Output, Request, Response, SimulationControlId, Stream},
    test_support::definition,
};
use seedbox_machine::{
    clock::ClockRate,
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
};
use std::{num::NonZeroU32, time::Duration};

fn at(nanos: u64) -> Timestamp {
    Timestamp::from_nanos(nanos)
}
fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}
fn says(text: &str) -> Response {
    Response::ApplicationOutput {
        stream: Stream::Stdout,
        bytes: text.as_bytes().to_vec(),
    }
}
fn call(n: u32, fields: &str) -> Response {
    Response::SimulationControl {
        id: SimulationControlId {
            session: NonZeroU32::new(1).unwrap(),
            request: NonZeroU32::new(n).unwrap(),
        },
        payload: format!("{{\"version\":1,\"id\":{n},{fields}}}").into_bytes(),
    }
}

/// A machine that emits its script at the scripted times after each boot,
/// later when it was not run in time, that acknowledges each process command
/// as it arrives, and whose application exits once the host hits a breakpoint
/// it waits on.
#[derive(Clone, Default)]
struct Speaker {
    script: Vec<(u64, Response)>,
    next: usize,
    booted: u64,
    crashed: bool,
    exiting: bool,
}
impl Machine for Speaker {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = context.clock.now();
        let mut commands = Vec::new();
        while let Some(input) = context.inbox.take() {
            match input.payload {
                Request::Application { id, .. } => commands.push(id),
                Request::Crash => self.crashed = true,
                Request::Reboot => {
                    self.crashed = false;
                    self.next = 0;
                    self.booted = now;
                }
                Request::SimulationReply { payload, .. } => {
                    let reply: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                    self.exiting |= reply["result"]["source"] == "host";
                }
                _ => {}
            }
        }
        if self.crashed {
            return Ok(StopReason::Exited {
                reached: at(now),
                reason: ExitReason::Crashed,
            });
        }
        let mut emit = |time: u64, payload| {
            let output = Output {
                at: at(time),
                payload,
            };
            context.outbox.emit(output).unwrap()
        };
        for id in commands {
            let result = CommandResult::Completed;
            emit(now, Response::CommandAcknowledged { id, result });
        }
        if std::mem::take(&mut self.exiting) {
            emit(now, Response::ApplicationOutcome { code: 0 });
        }
        while let Some((time, payload)) = self.script.get(self.next) {
            let due = self.booted + time;
            if due > upto.as_nanos() {
                break;
            }
            emit(due.max(now), payload.clone());
            self.next += 1;
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}

type Scripts = Vec<Vec<(u64, Response)>>;

fn engine() -> Engine {
    Engine {
        backend: seedbox_spec::backend::CpuBackend::Pv,
        architecture: seedbox_spec::definition::Architecture::Aarch64,
    }
}
fn driver(scripts: Scripts) -> Driver<Speaker> {
    let mut scripts = scripts.into_iter();
    let names = |index: usize| definition(format!("m{index}"));
    let mut driver = Driver::new_seeded(
        at(0),
        Duration::from_nanos(10),
        7,
        || Ok(()),
        names(0),
        (1..scripts.len()).map(names).collect(),
        vec![],
        seedbox_driver::test_support::floor(Duration::from_nanos(10)),
        move |_| {
            Ok(Speaker {
                script: scripts.next().unwrap(),
                ..Default::default()
            })
        },
    )
    .unwrap();
    driver.set_engine(engine());
    driver
}
fn run(scripts: Scripts, inputs: Vec<Input>) -> Run<Speaker> {
    Run::new(driver(scripts), Program::new(inputs).unwrap()).unwrap()
}
fn restore(
    checkpoint: &Checkpoint<Speaker>,
) -> Result<Driver<Speaker>, seedbox_driver::driver::Error<std::io::Error>> {
    let mut driver = Driver::from_checkpoint(
        checkpoint,
        || Ok(()),
        |saved: &Speaker| Ok(saved.clone()),
        |_| unreachable!("no spawns"),
    )?;
    driver.set_engine(engine());
    Ok(driver)
}
fn restored(branch: &Branch<Speaker>) -> Run<Speaker> {
    branch.restore(&mut restore).unwrap()
}
fn machine(index: usize) -> Target {
    Target::Name(format!("m{index}"))
}
fn id(run: &mut Run<Speaker>, index: usize) -> MachineId {
    run.barrier(|barrier| Ok(barrier.view()?.machines[index].id))
        .unwrap()
}
fn input(time: u64, name: &str, action: Action) -> Input {
    Input::action(at(time), key(name), action)
}
/// Each machine's lines, exits and transitions with their times.
fn occurrences(run: &Run<Speaker>) -> Vec<Record> {
    let mut records: Vec<Record> = run
        .trace()
        .records()
        .filter(|record| record.is_occurrence())
        .cloned()
        .collect();
    records.sort_by_key(|record| record.machine.map(MachineId::index));
    records
}
fn lines(run: &Run<Speaker>) -> Vec<(u64, String)> {
    run.trace()
        .records()
        .filter_map(|record| match &record.entry {
            Entry::Line { text, .. } => Some((record.at.as_nanos(), text.clone())),
            _ => None,
        })
        .collect()
}
/// An oracle that says nothing, and a server that talks.
fn talk() -> Scripts {
    vec![
        vec![],
        vec![
            (5, says("boot")),
            (25, says("elect")),
            (45, says("leader\n")),
            (65, says("commit 1\ncommit 2\n")),
            (85, says("done\n")),
        ],
    ]
}

#[test]
fn a_line_has_the_time_of_its_first_byte_and_ends_with_its_boot() {
    let crash = input(50, "crash", Action::Crash(machine(1)));
    let reboot = input(60, "reboot", Action::Reboot(machine(1)));
    let scripts = vec![
        vec![],
        vec![(5, says("hel")), (12, says("lo\nwor")), (25, says("ld"))],
    ];
    let mut run = run(scripts, vec![crash, reboot]);
    assert_eq!(run.advance(at(100)).unwrap(), Stop::Reached);
    let server = id(&mut run, 1);
    let seen: Vec<_> = run
        .trace()
        .records()
        .filter(|record| record.machine == Some(server))
        .filter_map(|record| match &record.entry {
            Entry::Line {
                text, truncated, ..
            } => Some((record.at.as_nanos(), record.boot, text.clone(), *truncated)),
            Entry::Transition { change, .. } => Some((
                record.at.as_nanos(),
                record.boot,
                format!("{change:?}"),
                false,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        seen,
        [
            (5, 0, "hello".into(), false),
            // The crash ended a line nobody finished.
            (12, 0, "world".into(), true),
            (50, 0, format!("{:?}", Transition::Crashed), false),
            (
                60,
                0,
                format!("{:?}", Transition::Rebooted { new_boot: 1 }),
                false
            ),
            (65, 1, "hello".into(), false),
        ]
    );
}

#[test]
fn a_restored_run_repeats_its_parent_until_its_own_inputs_differ() {
    let mut parent = run(talk(), vec![]);
    parent.advance(at(30)).unwrap();
    let saved = parent.save().unwrap();
    assert_eq!(saved.at(), at(30));
    parent.advance(at(100)).unwrap();
    let whole = lines(&parent);
    assert_eq!(
        whole,
        [
            (5, "bootelectleader".to_owned()),
            (65, "commit 1".into()),
            (65, "commit 2".into()),
            (85, "done".into())
        ]
    );
    // The same continuation, twice, in any order with the parent.
    for _ in 0..2 {
        let mut same = restored(&saved);
        assert_eq!(same.now(), at(30));
        same.advance(at(100)).unwrap();
        assert_eq!(occurrences(&same), occurrences(&parent));
    }
    // Its own input: paused across two lines, which it writes late.
    let mut other = restored(&saved);
    other
        .extend_pending([
            input(40, "pause", Action::Pause(machine(1))),
            input(70, "resume", Action::Resume(machine(1))),
        ])
        .unwrap();
    other.advance(at(100)).unwrap();
    assert_eq!(
        lines(&other),
        [
            (5, "bootelectleader".to_owned()),
            (70, "commit 1".into()),
            (70, "commit 2".into()),
            (85, "done".into())
        ]
    );
    // The parent and the saved point are as they were.
    assert_eq!(lines(&parent), whole);
    assert!(saved.program().delivered().is_empty());
    assert_eq!(other.program().delivered().len(), 2);
}

/// The oracle asks at 13 for the server to crash, and the Driver applies a
/// guest's call one floor (10 ns) after it, at 23, after the server's line at
/// 17. Stopping exactly at every nanosecond and saving there changes nothing.
#[test]
fn where_a_run_stops_or_saves_does_not_change_what_its_machines_do() {
    let scripts = || {
        vec![
            vec![(13, call(1, r#""op":"crash","target":{"machine":"m1"}"#))],
            vec![(5, says("boot\n")), (17, says("late\n"))],
        ]
    };
    let mut straight = run(scripts(), vec![]);
    straight.advance(at(100)).unwrap();
    assert_eq!(
        lines(&straight),
        [(5, "boot".to_owned()), (17, "late".into())]
    );

    let mut stepped = run(scripts(), vec![]);
    let mut saves = Vec::new();
    for time in 1..=100 {
        assert_eq!(stepped.advance(at(time)).unwrap(), Stop::Reached);
        assert_eq!(stepped.now(), at(time));
        saves.push(stepped.save().unwrap());
    }
    assert_eq!(lines(&stepped), lines(&straight));
    assert_eq!(occurrences(&stepped), occurrences(&straight));
    // A run restored from any of those points goes on the same way, among
    // them the ones between the call and the crash, and before the line.
    for saved in [&saves[12], &saves[14], &saves[16], &saves[21]] {
        let mut resumed = restored(saved);
        resumed.advance(at(100)).unwrap();
        assert_eq!(occurrences(&resumed), occurrences(&straight));
    }
}

/// The server's line at 20 is written by the round that ends at 20 and held
/// there, since the oracle can still write at 20. The host breakpoint due at
/// 20 is admitted there, which commits the line. It is in the trace at once,
/// and in the trace of a run restored from a save taken there: a checkpoint
/// carries no published output, so the run could not recover it later.
#[test]
fn a_save_where_a_host_breakpoint_commits_held_output_keeps_that_output() {
    let scripts = || {
        vec![
            vec![],
            vec![
                (5, says("boot\n")),
                (20, says("held\n")),
                (45, says("after\n")),
            ],
        ]
    };
    let cut = vec![Input::host_break(at(20), key("cut"), "unheard")];
    let mut parent = run(scripts(), cut);
    assert_eq!(parent.advance(at(20)).unwrap(), Stop::Reached);
    assert_eq!(parent.now(), at(20));
    let so_far = [(5, "boot".to_owned()), (20, "held".into())];
    assert_eq!(lines(&parent), so_far);
    let saved = parent.save().unwrap();
    parent.advance(at(100)).unwrap();
    let mut child = restored(&saved);
    assert_eq!(lines(&child), so_far);
    child.advance(at(100)).unwrap();
    assert_eq!(
        lines(&child),
        [
            (5, "boot".to_owned()),
            (20, "held".into()),
            (45, "after".into())
        ]
    );
    assert_eq!(occurrences(&child), occurrences(&parent));
}

#[test]
fn rewind_returns_to_just_before_a_line_the_run_already_wrote() {
    // The pause delays the commit lines to 70; only this run knows that.
    let inputs = vec![
        input(40, "pause", Action::Pause(machine(1))),
        input(70, "resume", Action::Resume(machine(1))),
        input(90, "late", Action::Pause(machine(1))),
    ];
    let mut scout = run(talk(), inputs);
    scout.advance(at(20)).unwrap();
    let early = scout.save().unwrap();
    scout.advance(at(100)).unwrap();
    let commit = scout
        .trace()
        .records()
        .find(|record| matches!(&record.entry, Entry::Line { text, .. } if text == "commit 1"))
        .unwrap()
        .clone();
    assert_eq!(commit.at, at(70));

    let mut before = early
        .rewind(scout.history(), at(commit.at.as_nanos() - 3), &mut restore)
        .unwrap();
    assert_eq!(before.now(), at(67));
    // It took the scout's inputs up to there and holds none of the later ones.
    let delivered: Vec<&str> = before
        .program()
        .delivered()
        .iter()
        .map(|input| input.key.as_str())
        .collect();
    assert_eq!(delivered, ["pause"]);
    assert!(before.program().pending().is_empty());
    // The server was paused before it ended its first line.
    assert!(lines(&before).is_empty());
    // What follows is this run's own: without the resume no line ever ends.
    before.advance(at(100)).unwrap();
    assert!(lines(&before).is_empty());

    // Replaying the scout's own suffix from the same place writes the line
    // where the scout saw it.
    let mut again = early.rewind(scout.history(), at(67), &mut restore).unwrap();
    again
        .extend_pending([input(70, "resume", Action::Resume(machine(1)))])
        .unwrap();
    again.advance(at(80)).unwrap();
    // The line begun before the saved point ends here, with its first time.
    assert_eq!(
        lines(&again),
        [
            (5, "bootelectleader".to_owned()),
            (70, "commit 1".into()),
            (70, "commit 2".into())
        ]
    );
}

#[test]
fn rewind_delivers_an_input_due_at_the_point_it_returns_to() {
    // The inputs arrive after the saved point, the first due at that point.
    let mut scout = run(talk(), vec![]);
    scout.advance(at(39)).unwrap();
    let earlier = scout.save().unwrap();
    scout.advance(at(40)).unwrap();
    let saved = scout.save().unwrap();
    scout
        .extend_pending([
            input(40, "pause", Action::Pause(machine(1))),
            input(70, "resume", Action::Resume(machine(1))),
        ])
        .unwrap();
    scout.advance(at(100)).unwrap();
    let history = scout.history();
    let keys = |run: &Run<Speaker>| -> Vec<String> {
        let delivered = run.program().delivered().iter();
        delivered
            .map(|input| input.key.as_str().to_owned())
            .collect()
    };

    // Back at the saved point the pause is in force, as it was in the scout.
    let mut paused = saved.rewind(history, at(40), &mut restore).unwrap();
    assert_eq!(paused.now(), at(40));
    assert_eq!(keys(&paused), ["pause"]);
    assert!(paused.program().pending().is_empty());
    paused.advance(at(100)).unwrap();
    assert!(lines(&paused).is_empty());

    // A nanosecond earlier it is not, and the server ends its line on time.
    let mut free = earlier.rewind(history, at(39), &mut restore).unwrap();
    assert!(keys(&free).is_empty());
    free.advance(at(50)).unwrap();
    assert_eq!(lines(&free), [(5, "bootelectleader".to_owned())]);

    // The resume is due where this rewind stops, so the scout's lines follow.
    let mut resumed = saved.rewind(history, at(70), &mut restore).unwrap();
    assert_eq!(keys(&resumed), ["pause", "resume"]);
    resumed.advance(at(100)).unwrap();
    assert_eq!(occurrences(&resumed), occurrences(&scout));
}

#[test]
fn rewind_refuses_a_replay_missing_a_line_the_history_wrote() {
    // The history ended a line at 75 that the replay never writes.
    let mut scripts = talk();
    scripts[1].insert(4, (75, says("gone\n")));
    let mut imposter = run(scripts, vec![]);
    imposter.advance(at(100)).unwrap();
    let mut honest = run(talk(), vec![]);
    honest.advance(at(20)).unwrap();
    let early = honest.save().unwrap();
    match early.rewind(imposter.history(), at(80), &mut restore) {
        Err(Error::Diverged {
            expected,
            found: None,
        }) => assert!(matches!(
            expected.unwrap().entry,
            Entry::Line { text, .. } if text == "gone"
        )),
        other => panic!("{:?}", other.map(|run| run.now())),
    }
    // Before the line began, nothing is missing yet.
    assert!(early
        .rewind(imposter.history(), at(75), &mut restore)
        .is_ok());
}

/// At nine tenths of simulation time, simulation nanoseconds 10 and 11
/// share machine nanosecond 9. A run stopped at 11 has not committed what
/// its machines stamped at 10: the slow machine could still write there.
/// Rewind compares up to the Driver's committed frontier, not up to 11.
#[test]
fn rewind_under_a_slow_clock_compares_up_to_what_the_replay_committed() {
    let slow = || {
        input(
            0,
            "slow",
            Action::Drift {
                target: machine(1),
                rate: ClockRate::new(-100_000_000).unwrap(),
            },
        )
    };
    // Machine times 9 and 45: stamped 10 and 50.
    let scripts = || vec![vec![], vec![(9, says("first\n")), (45, says("second\n"))]];
    let mut scout = run(scripts(), vec![slow()]);
    scout.advance(at(5)).unwrap();
    let early = scout.save().unwrap();
    scout.advance(at(100)).unwrap();
    assert_eq!(
        lines(&scout),
        [(10, "first".to_owned()), (50, "second".into())]
    );

    // Stopped at 11, the replay holds the line stamped 10 uncommitted.
    let mut before = early.rewind(scout.history(), at(11), &mut restore).unwrap();
    assert_eq!(before.now(), at(11));
    let frontier = before.barrier(|barrier| Ok(barrier.committed_before()));
    assert_eq!(frontier.unwrap(), Some(at(10)));
    assert!(lines(&before).is_empty());
    // A run restored from a save there holds it too, and says so.
    let mut saved = restored(&before.save().unwrap());
    let frontier = saved.barrier(|barrier| Ok(barrier.committed_before()));
    assert_eq!(frontier.unwrap(), Some(at(10)));
    saved.advance(at(100)).unwrap();
    assert_eq!(occurrences(&saved), occurrences(&scout));
    // It is the scout's run: the line follows, where the scout saw it.
    before.advance(at(100)).unwrap();
    assert_eq!(occurrences(&before), occurrences(&scout));
    // One nanosecond on, nothing can be stamped at 10 any more.
    let mut after = early.rewind(scout.history(), at(12), &mut restore).unwrap();
    assert_eq!(lines(&after), [(10, "first".to_owned())]);
    let frontier = after.barrier(|barrier| Ok(barrier.committed_before()));
    assert_eq!(frontier.unwrap(), Some(at(12)));

    // Stopped at every nanosecond, the frontier trails the stop by at most
    // one, never goes back, and never passes a record the trace has yet to
    // receive.
    let mut stepped = restored(&early);
    let (mut frontier, mut seen) = (stepped.now(), stepped.trace().len());
    for time in 6..=60 {
        stepped.advance(at(time)).unwrap();
        assert!(stepped.trace().since(seen).all(|new| new.at >= frontier));
        let next = stepped.barrier(|barrier| Ok(barrier.committed_before()));
        let next = next.unwrap().unwrap();
        assert!(next >= frontier && next >= at(time - 1) && next <= at(time));
        (frontier, seen) = (next, stepped.trace().len());
    }
    assert_eq!(lines(&stepped), lines(&scout));

    // A history with a line stamped 9, before the frontier, that the replay
    // never writes is still refused, also when rewinding to 11.
    let mut scripts = scripts();
    scripts[1].insert(0, (8, says("gone\n")));
    let mut imposter = run(scripts, vec![slow()]);
    imposter.advance(at(100)).unwrap();
    assert_eq!(lines(&imposter)[0], (9, "gone".to_owned()));
    match early.rewind(imposter.history(), at(11), &mut restore) {
        Err(Error::Diverged {
            expected,
            found: None,
        }) => assert!(matches!(
            expected.unwrap().entry,
            Entry::Line { text, .. } if text == "gone"
        )),
        other => panic!("{:?}", other.map(|run| run.now())),
    }
}

#[test]
fn rewind_refuses_a_branch_of_another_run_and_a_replay_that_differs() {
    let mut scout = run(talk(), vec![input(40, "pause", Action::Pause(machine(1)))]);
    scout.advance(at(50)).unwrap();
    let late = scout.save().unwrap();
    scout.advance(at(100)).unwrap();
    // Before the branch's own time.
    assert!(matches!(
        late.rewind(scout.history(), at(45), &mut restore),
        Err(Error::NotOnHistory)
    ));
    // A run that took another input is not this branch's future.
    let mut stranger = run(talk(), vec![input(10, "other", Action::Pause(machine(1)))]);
    stranger.advance(at(100)).unwrap();
    assert!(matches!(
        late.rewind(stranger.history(), at(80), &mut restore),
        Err(Error::NotOnHistory)
    ));
    // The same inputs over machines that say something else: the replay is
    // checked against the history and refused.
    let mut scripts = talk();
    scripts[1][3] = (65, says("commit 9\n"));
    let mut imposter = run(scripts, vec![]);
    imposter.advance(at(100)).unwrap();
    let mut honest = run(talk(), vec![]);
    honest.advance(at(20)).unwrap();
    let early = honest.save().unwrap();
    match early.rewind(imposter.history(), at(80), &mut restore) {
        Err(Error::Diverged { expected, found }) => {
            let text = |record: Option<Box<Record>>| match record.unwrap().entry {
                Entry::Line { text, .. } => text,
                other => panic!("{other:?}"),
            };
            assert_eq!(text(expected), "commit 9");
            assert_eq!(text(found), "commit 1");
        }
        other => panic!("{:?}", other.map(|run| run.now())),
    }
}

#[test]
fn recover_ends_every_fault_and_drops_what_was_still_to_come() {
    let scripts = vec![vec![], vec![], vec![], vec![]];
    let throttle = Action::Throttle {
        target: machine(3),
        config: seedbox_machine::execution::ExecutionConfig::new(2000).unwrap(),
    };
    let inputs = vec![
        input(10, "pause", Action::Pause(machine(1))),
        input(10, "crash", Action::Crash(machine(2))),
        input(10, "throttle", throttle),
        input(500, "never", Action::Crash(machine(3))),
    ];
    let mut run = run(scripts, inputs);
    run.advance(at(30)).unwrap();
    let view = run.barrier(|barrier| barrier.view()).unwrap();
    let states: Vec<MachineState> = view.machines.iter().map(|m| m.state).collect();
    assert_eq!(
        states,
        [
            MachineState::Running,
            MachineState::Paused,
            MachineState::Down,
            MachineState::Running
        ]
    );
    assert_eq!(view.handles.len(), 1);
    let recovered = run.recover("r-", Duration::from_nanos(10), 4).unwrap();
    assert!(recovered.clean);
    assert_eq!(recovered.dropped, [key("never")]);
    let view = run.barrier(|barrier| barrier.view()).unwrap();
    assert!(view.handles.is_empty());
    assert!(view
        .machines
        .iter()
        .all(|machine| machine.state == MachineState::Running));
    // Every undo is an input of the program; the future crash is gone.
    let keys: Vec<&str> = run
        .program()
        .delivered()
        .iter()
        .map(|input| input.key.as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "pause",
            "crash",
            "throttle",
            "r-0-heal-0",
            "r-0-resume-1",
            "r-0-reboot-2"
        ]
    );
    assert!(run.program().pending().is_empty());
}

/// An oracle that declares `finish` and waits for it, beside a server.
fn waiting_oracle() -> Scripts {
    vec![
        vec![
            (1, call(1, r#""op":"declare_breaks","names":["finish"]"#)),
            (2, call(2, r#""op":"wait_break","timeout_ns":1000000000"#)),
        ],
        vec![(5, says("serving\n"))],
    ]
}

#[test]
fn judge_recovers_tells_the_oracle_to_finish_and_reads_its_exit() {
    let mut scout = run(
        waiting_oracle(),
        vec![input(10, "pause", Action::Pause(machine(1)))],
    );
    scout.advance(at(30)).unwrap();
    let saved = scout.save().unwrap();
    let exit = ExitReason::ProgramExit { code: 0 };
    for _ in 0..2 {
        let mut judged = restored(&saved);
        judged
            .extend_pending([input(500, "never", Action::Crash(machine(1)))])
            .unwrap();
        let verdict = judged
            .judge(
                "finish",
                Duration::from_nanos(10),
                Duration::from_nanos(500),
            )
            .unwrap();
        assert!(verdict.recovered && verdict.signalled);
        assert_eq!(verdict.finished, Some(exit));
        // Recovery took one look of 10 ns; the oracle exited in the next round.
        assert!(verdict.at <= at(60), "{:?}", verdict.at);
        // The input it never took is named, and the oracle heard the first ask.
        assert_eq!(verdict.dropped, [key("never")]);
        assert!(judged.missed().is_empty());
        let delivered = judged.program().delivered();
        let keys: Vec<&str> = delivered.iter().map(|input| input.key.as_str()).collect();
        assert_eq!(keys, ["pause", "judge-0-resume-1", "judge-finish-0"]);

        // The judged run's program alone, from boot, ends the same way at the
        // same time.
        let mut cold = run(waiting_oracle(), delivered.to_vec());
        assert_eq!(cold.advance(at(1000)).unwrap(), Stop::Finished(exit));
        assert_eq!(cold.now(), verdict.at);
        assert_eq!(occurrences(&cold), occurrences(&judged));
    }
    // The scout was not spent: it is still running, still paused.
    scout.advance(at(100)).unwrap();
    assert_eq!(scout.finished(), None);
}

#[test]
fn judge_waits_out_an_oracle_that_never_declares_the_breakpoint() {
    let mut run = run(talk(), vec![]);
    run.advance(at(30)).unwrap();
    let verdict = run
        .judge(
            "finish",
            Duration::from_nanos(10),
            Duration::from_nanos(200),
        )
        .unwrap();
    assert!(verdict.recovered && !verdict.signalled);
    assert_eq!((verdict.finished, verdict.at), (None, at(230)));
    // It asked every 10 ns from 30 to 220, and each ask is on record as missed.
    let asks: Vec<&str> = run.missed().iter().map(InputKey::as_str).collect();
    assert_eq!(asks.len(), 20);
    assert_eq!((asks[0], asks[19]), ("judge-finish-0", "judge-finish-19"));
    assert_eq!(run.program().delivered().len(), 20);
}

/// [`waiting_oracle`] with a server that is ready for process commands.
fn ready_server() -> Scripts {
    let mut scripts = waiting_oracle();
    scripts[1].insert(0, (0, Response::Ready));
    scripts
}
/// The server has its application frozen at 10 and its machine paused at 20,
/// so the pause hides the freeze.
fn frozen_then_paused() -> Run<Speaker> {
    let freeze = Action::PauseProcess {
        target: machine(1),
        deadline: Duration::from_nanos(1000),
    };
    let inputs = vec![
        input(10, "freeze", freeze),
        input(20, "pause", Action::Pause(machine(1))),
    ];
    let mut run = run(ready_server(), inputs);
    run.advance(at(30)).unwrap();
    run
}
fn state(run: &mut Run<Speaker>, index: usize) -> MachineState {
    run.barrier(|barrier| Ok(barrier.view()?.machines[index].state))
        .unwrap()
}
fn delivered(run: &Run<Speaker>) -> Vec<String> {
    let delivered = run.program().delivered().iter();
    delivered
        .map(|input| input.key.as_str().to_owned())
        .collect()
}

#[test]
fn recover_and_judge_look_again_for_what_a_pause_hid() {
    let mut scout = frozen_then_paused();
    assert_eq!(state(&mut scout, 1), MachineState::Paused);
    let saved = scout.save().unwrap();
    let settle = Duration::from_nanos(10);

    // One look resumes the machine and finds its application still frozen.
    let mut short = restored(&saved);
    assert!(!short.recover("r-", settle, 1).unwrap().clean);
    assert_eq!(state(&mut short, 1), MachineState::Frozen);

    // A second look thaws it.
    let mut enough = restored(&saved);
    assert!(enough.recover("r-", settle, 4).unwrap().clean);
    assert_eq!(state(&mut enough, 1), MachineState::Running);
    assert_eq!(
        delivered(&enough),
        ["freeze", "pause", "r-0-resume-1", "r-1-thaw-1"]
    );

    // Judging takes the same two looks before it asks the oracle to finish,
    // and its program alone reproduces the verdict from boot.
    let mut judged = restored(&saved);
    let verdict = judged
        .judge("finish", settle, Duration::from_nanos(500))
        .unwrap();
    let exit = ExitReason::ProgramExit { code: 0 };
    assert!(verdict.recovered && verdict.signalled);
    assert_eq!(verdict.finished, Some(exit));
    assert_eq!(
        delivered(&judged),
        [
            "freeze",
            "pause",
            "judge-0-resume-1",
            "judge-1-thaw-1",
            "judge-finish-0"
        ]
    );
    let mut cold = run(ready_server(), judged.program().delivered().to_vec());
    assert_eq!(cold.advance(at(1000)).unwrap(), Stop::Finished(exit));
    assert_eq!(cold.now(), verdict.at);
    assert_eq!(occurrences(&cold), occurrences(&judged));
}

#[test]
fn recover_refuses_a_label_that_makes_an_invalid_key() {
    let mut run = frozen_then_paused();
    run.extend_pending([input(500, "later", Action::Crash(machine(1)))])
        .unwrap();
    // The last one is valid alone but too long for the keys it would make.
    let long = "r".repeat(120);
    for label in ["not a key", "slash/", &long] {
        assert!(matches!(
            run.recover(label, Duration::from_nanos(10), 4),
            Err(Error::Label(_))
        ));
    }
    // It refused before changing anything.
    assert_eq!(delivered(&run), ["freeze", "pause"]);
    assert_eq!(run.program().pending().len(), 1);
    assert_eq!(state(&mut run, 1), MachineState::Paused);
}

#[test]
fn recover_and_judge_refuse_a_zero_settle() {
    let mut run = run(talk(), vec![]);
    run.advance(at(30)).unwrap();
    assert!(matches!(
        run.recover("r-", Duration::ZERO, 4),
        Err(Error::NoSettle)
    ));
    assert!(matches!(
        run.judge("finish", Duration::ZERO, Duration::from_nanos(200)),
        Err(Error::NoSettle)
    ));
    // Nothing was asked or scheduled.
    assert!(run.missed().is_empty());
    assert!(run.program().delivered().is_empty());
}

#[test]
fn a_breakpoint_no_machine_declared_is_reported_as_missed() {
    // The oracle declares `finish` at 1 ns and nothing else, ever.
    let inputs = vec![
        Input::host_break(at(0), key("early"), "finish"),
        Input::host_break(at(10), key("other"), "start"),
        Input::host_break(at(20), key("heard"), "finish"),
    ];
    let mut scout = run(waiting_oracle(), inputs);
    scout.advance(at(15)).unwrap();
    assert_eq!(scout.missed(), [key("early"), key("other")]);
    // A saved point remembers them, and the run restored from it adds none
    // for the breakpoint the oracle was waiting on.
    let saved = scout.save().unwrap();
    let mut heard = restored(&saved);
    let exit = ExitReason::ProgramExit { code: 0 };
    assert_eq!(heard.advance(at(100)).unwrap(), Stop::Finished(exit));
    assert_eq!(heard.missed(), [key("early"), key("other")]);
}

#[test]
fn a_branch_is_reproduced_by_its_program_from_boot() {
    // Three saves deep, each restored run adding inputs of its own.
    let mut root = run(talk(), vec![input(15, "a", Action::Pause(machine(1)))]);
    root.advance(at(23)).unwrap();
    let first = root.save().unwrap();
    let mut second = restored(&first);
    second
        .extend_pending([input(31, "b", Action::Resume(machine(1)))])
        .unwrap();
    second.advance(at(47)).unwrap();
    let saved = second.save().unwrap();
    let mut third = restored(&saved);
    third
        .extend_pending([
            input(52, "c", Action::Crash(machine(1))),
            input(58, "d", Action::Reboot(machine(1))),
        ])
        .unwrap();
    third.advance(at(71)).unwrap();
    let leaf = third.save().unwrap();
    let mut end = restored(&leaf);
    end.advance(at(200)).unwrap();

    // The leaf's program alone, from boot, in one stride.
    let mut cold = run(talk(), end.program().delivered().to_vec());
    cold.advance(at(200)).unwrap();
    assert_eq!(occurrences(&cold), occurrences(&end));
    assert!(lines(&cold).len() > 3);
    // And stopping somewhere else on the way changes nothing either.
    let mut halting = run(talk(), end.program().delivered().to_vec());
    for stop in [3, 16, 29, 30, 31, 55, 58, 64, 111, 200] {
        halting.advance(at(stop)).unwrap();
        let _ = halting.save().unwrap();
    }
    assert_eq!(occurrences(&halting), occurrences(&end));
}

/// The oracle crashes the server at 13 ns; the server writes at 5 and 17.
fn crash_scripts() -> Scripts {
    vec![
        vec![(13, call(1, r#""op":"crash","target":{"machine":"m1"}"#))],
        vec![(5, says("boot\n")), (17, says("late\n"))],
    ]
}

#[test]
fn a_watch_is_asked_every_stride_and_where_it_stops_changes_nothing() {
    // By default the watch is asked every quantum (10 ns) up to the end.
    let mut every = run(crash_scripts(), vec![]);
    let mut asked = Vec::new();
    let mut seen = Vec::new();
    let stop = every.advance_watching(at(100), None, |trace, _| {
        asked.push(trace.len());
        false
    });
    assert_eq!(stop.unwrap(), Stop::Reached);
    assert_eq!(asked.len(), 10);
    // With a stride of 4 ns from 2 ns, it is asked at 6, 10, 14 ... and
    // stops at the first look that finds a new line: the server's boot line
    // ends at 5, so at 6, before the crash the oracle asked for at 13.
    let mut watched = run(crash_scripts(), vec![]);
    watched.advance(at(2)).unwrap();
    let stride = Some(Duration::from_nanos(4));
    let stop = watched.advance_watching(at(100), stride, |trace, first| {
        seen.push(trace.len());
        trace
            .since(first)
            .any(|record| matches!(record.entry, Entry::Line { .. }))
    });
    assert_eq!(stop.unwrap(), Stop::Watched);
    assert_eq!(watched.now(), at(6));
    assert_eq!(seen.len(), 1);
    // A watch's stop is no input: the program is unchanged, and the run goes
    // on as one that never stopped.
    assert!(watched.program().delivered().is_empty());
    watched.advance(at(100)).unwrap();
    assert_eq!(occurrences(&watched), occurrences(&every));
}
