use super::*;
use crate::driver::fixture;
#[test]
fn duplicate_display_names_never_alias_sources_and_unnamed_additions_route() {
    let mut control = Control::new(&[fixture::definition(0), fixture::definition(0)], &[0, 1]);
    assert!(control.target("fixture-0").is_err());
    for index in 0..2 {
        assert_eq!(
            control
                .reply_target(&Control::source(MachineId(index)))
                .unwrap(),
            MachineId(index)
        );
    }
    control.add_unnamed(2);
    assert_eq!(
        control
            .reply_target(&Control::source(MachineId(2)))
            .unwrap(),
        MachineId(2)
    );
    assert!(control.target("machine:2").is_err());
}
#[test]
fn boot_allocator_restarts_per_boot_and_never_wraps() {
    let mut commands = Commands::default();
    assert_eq!(
        commands
            .reserve(
                7,
                CommandKind::Application(crate::pv::ApplicationCommand::Stop)
            )
            .unwrap()
            .get(),
        1
    );
    commands.reset();
    assert_eq!(
        commands
            .reserve(
                8,
                CommandKind::Application(crate::pv::ApplicationCommand::Start)
            )
            .unwrap()
            .get(),
        1
    );
    commands.next = u32::MAX;
    let before = commands.used.clone();
    assert!(commands
        .reserve(
            9,
            CommandKind::Application(crate::pv::ApplicationCommand::Pause)
        )
        .is_err());
    assert_eq!(commands.used, before);
}
/// A natural application exit shows in guest inspect while its machine stays
/// up, as the world API reports it, and a later Start or Stop replaces it.
#[test]
fn inspect_reports_an_application_exit_while_its_machine_runs() {
    use crate::driver::fixture::owned::*;
    let inspect = |n, time| (time, call(n, r#""op":"inspect""#));
    let mut oracle = vec![(0, crate::pv::Response::Ready)];
    oracle.extend([
        inspect(1, 5),
        inspect(2, 45),
        inspect(3, 65),
        inspect(4, 85),
    ]);
    // The target keeps running after its application's leader exits.
    let target = vec![
        (0, crate::pv::Response::Ready),
        (30, crate::pv::Response::ApplicationOutcome { code: 3 }),
    ];
    let mut d = build(
        vec![spec(0, true, oracle), spec(1, true, target)],
        vec![],
        true,
    );
    use crate::driver::{Action, Target};
    let target = || Target::Machine(MachineId(1));
    let deadline = std::time::Duration::from_nanos(1_000);
    let restart = Action::Restart {
        target: target(),
        deadline,
    };
    let stop = Action::Stop {
        target: target(),
        deadline,
    };
    d.schedule(at(50), key("restart"), restart).unwrap();
    d.schedule(at(70), key("stop"), stop).unwrap();
    while d.step(at(120)).unwrap() != (crate::driver::StepOutcome::BarrierReached { at: at(120) }) {
    }
    let states: Vec<String> = states(&d)[0]
        .received
        .iter()
        .filter_map(|input| match &input.payload {
            crate::pv::Request::SimulationReply { payload, .. } => {
                let reply: Value = serde_json::from_slice(payload).unwrap();
                let machines = reply["result"]["machines"].as_array().unwrap().clone();
                let target = machines.iter().find(|m| m["name"] == "fixture-1").unwrap();
                Some(target["state"].as_str().unwrap().to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(states, ["Running", "Exited(3)", "Running", "Stopped"]);
}
