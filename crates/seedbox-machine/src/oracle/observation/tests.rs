use super::*;
fn state() -> State {
    State::new("peer".into(), 0, 0, ExecutionConfig::default())
}
fn id(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}
fn ready(s: &mut State, at: u64) {
    s.response(s.boot, at, at, &Response::Ready).unwrap();
}
fn command(s: &mut State, n: u32, action: ApplicationCommand, at: u64) {
    s.command_admitted(s.boot, id(n), action, Origin::External, at, at)
        .unwrap();
}
fn ack(s: &mut State, n: u32, result: CommandResult, observed: u64, commit: u64) {
    s.response(
        s.boot,
        observed,
        commit,
        &Response::CommandAcknowledged { id: id(n), result },
    )
    .unwrap();
}
#[test]
fn natural_application_outcome_is_not_commanded_stop_or_logical_crash() {
    let mut s = state();
    ready(&mut s, 1);
    s.response(0, 7, 10, &Response::ApplicationOutcome { code: 23 })
        .unwrap();
    let facts = s.facts().unwrap();
    assert_eq!(facts.vm, Vm::Up);
    assert_eq!(facts.process, Process::Stopped);
    assert!(!facts.physically_ended);
    assert_eq!(
        s.view().unwrap().application,
        ApplicationView {
            condition: ApplicationCondition::Exited,
            changed_at: 7,
            exit_code: Some(23)
        }
    );
    s.terminal(0, 8, 10, ExitReason::ProgramExit { code: 23 })
        .unwrap();
    let view = s.view().unwrap();
    assert_eq!(
        view.first_terminal,
        Some(Terminal {
            at: 8,
            reason: ExitReason::ProgramExit { code: 23 }
        })
    );
    assert_eq!(
        view.natural_outcome,
        Some(NaturalOutcome { at: 7, code: 23 })
    );
    assert_eq!(view.application.changed_at, 7);
    assert_eq!(s.facts().unwrap().process, Process::Stopped);
}
#[test]
fn explicit_stop_and_later_start_use_known_actions_and_no_fake_outcome() {
    let mut s = state();
    ready(&mut s, 1);
    command(&mut s, 99, ApplicationCommand::Stop, 2);
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Running
    );
    ack(&mut s, 99, CommandResult::Completed, 4, 10);
    assert_eq!(s.facts().unwrap().process, Process::Stopped);
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Stopped
    );
    assert_eq!(s.view().unwrap().application.changed_at, 4);
    assert_eq!(s.view().unwrap().natural_outcome, None);
    command(&mut s, 1, ApplicationCommand::Start, 11);
    ack(&mut s, 1, CommandResult::Rejected, 12, 12);
    assert_eq!(s.view().unwrap().application.changed_at, 4);
    command(&mut s, 7, ApplicationCommand::Start, 13);
    ack(&mut s, 7, CommandResult::Completed, 14, 20);
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Running
    );
    assert_eq!(s.facts().unwrap().process, Process::Running);
}
#[test]
fn external_ids_need_not_be_admitted_monotonically() {
    let mut s = state();
    ready(&mut s, 1);
    command(&mut s, 100, ApplicationCommand::Pause, 2);
    s.command_admitted(
        0,
        id(1),
        ApplicationCommand::Resume,
        Origin::Service { fault: 8 },
        3,
        3,
    )
    .unwrap();
    assert!(matches!(
        s.command_owner(0, id(100)).unwrap(),
        CommandOwner::External
    ));
    assert!(matches!(
        s.command_owner(0, id(1)).unwrap(),
        CommandOwner::Service {
            fault: 8,
            action: ApplicationCommand::Resume
        }
    ));
    ack(&mut s, 100, CommandResult::Completed, 4, 5);
    assert_eq!(s.facts().unwrap().process, Process::Frozen);
    ack(&mut s, 1, CommandResult::Completed, 5, 5);
    assert_eq!(s.facts().unwrap().process, Process::Running);
    assert!(s.commands.is_empty());
}
#[test]
fn host_pause_is_separate_from_application_pause_and_execution_setting() {
    let mut s = state();
    ready(&mut s, 1);
    command(&mut s, 1, ApplicationCommand::Pause, 2);
    ack(&mut s, 1, CommandResult::Completed, 3, 3);
    s.admission(0, 4, 4, Admission::Applied(Transition::Pause))
        .unwrap();
    let before = s.view().unwrap();
    assert_eq!(before.machine, MachineCondition::Paused);
    assert_eq!(before.application.condition, ApplicationCondition::Paused);
    s.admission(
        0,
        5,
        5,
        Admission::Applied(Transition::Execution(ExecutionConfig::new(2000).unwrap())),
    )
    .unwrap();
    let view = s.view().unwrap();
    assert_eq!(view.condition_since, 4);
    assert_eq!(view.application.changed_at, 3);
    assert_eq!(s.facts().unwrap().vm, Vm::Paused);
    s.admission(0, 6, 6, Admission::Applied(Transition::Resume))
        .unwrap();
    assert_eq!(s.view().unwrap().machine, MachineCondition::Ready);
    assert_eq!(s.facts().unwrap().process, Process::Frozen);
    assert_eq!(s.view().unwrap().application.changed_at, 3);
}
#[test]
fn logical_crash_preserves_sticky_program_exit_then_reboot_clears_boot_view() {
    let mut s = state();
    ready(&mut s, 1);
    s.admission(
        0,
        2,
        2,
        Admission::Applied(Transition::Execution(ExecutionConfig::new(2000).unwrap())),
    )
    .unwrap();
    s.response(0, 3, 3, &Response::ApplicationOutcome { code: 0 })
        .unwrap();
    s.terminal(0, 4, 4, ExitReason::ProgramExit { code: 0 })
        .unwrap();
    s.admission(0, 5, 5, Admission::Applied(Transition::Crash))
        .unwrap();
    assert_eq!(s.facts().unwrap().vm, Vm::Down);
    assert_eq!(s.view().unwrap().machine, MachineCondition::Down);
    assert_eq!(s.view().unwrap().first_terminal.unwrap().at, 4);
    assert_eq!(
        s.view().unwrap().first_terminal.unwrap().reason,
        ExitReason::ProgramExit { code: 0 }
    );
    assert!(matches!(
        s.command_owner(0, id(42)).unwrap(),
        CommandOwner::RetiredBoot
    ));
    s.admission(
        0,
        6,
        6,
        Admission::Applied(Transition::Reboot { new_boot: 1 }),
    )
    .unwrap();
    let view = s.view().unwrap();
    assert_eq!(view.boot, 1);
    assert_eq!(view.machine, MachineCondition::Starting);
    assert_eq!(view.application.condition, ApplicationCondition::Starting);
    assert_eq!(view.first_terminal, None);
    assert_eq!(view.natural_outcome, None);
    assert_eq!(view.execution.factor_permille(), 2000);
    assert_eq!(
        s.response(0, 3, 7, &Response::ApplicationOutcome { code: 99 }),
        Ok(Change::Retired)
    );
    assert_eq!(s.view().unwrap(), view);
    s.response(1, 7, 7, &Response::Ready).unwrap();
    assert_eq!(s.view().unwrap().machine, MachineCondition::Ready);
}
#[test]
fn crash_policy_does_not_invent_physical_terminal_time() {
    let mut s = state();
    ready(&mut s, 1);
    command(&mut s, 5, ApplicationCommand::Stop, 2);
    s.admission(0, 3, 3, Admission::Applied(Transition::Crash))
        .unwrap();
    assert!(s.commands.is_empty());
    assert_eq!(s.view().unwrap().first_terminal, None);
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Unavailable
    );
    s.terminal(0, 3, 4, ExitReason::Crashed).unwrap();
    assert_eq!(s.view().unwrap().first_terminal.unwrap().at, 3);
    assert_eq!(s.view().unwrap().machine, MachineCondition::Down);
    assert_eq!(s.view().unwrap().condition_since, 3);
}
#[test]
fn first_terminal_is_not_replaced_by_later_zero_budget_adoption() {
    let mut s = state();
    ready(&mut s, 1);
    s.terminal(0, 4, 10, ExitReason::Shutdown).unwrap();
    let first = s.view().unwrap();
    assert_eq!(
        s.terminal(0, 100, 100, ExitReason::Shutdown),
        Ok(Change::Unchanged)
    );
    assert_eq!(s.view().unwrap(), first);
    assert_eq!(s.facts().unwrap().vm, Vm::Up);
    assert_eq!(s.facts().unwrap().process, Process::Running);
    assert_eq!(s.view().unwrap().natural_outcome, None);
}
#[test]
fn rejected_no_effect_and_invalid_admissions_preserve_projection() {
    let mut s = state();
    ready(&mut s, 1);
    let before = s.view().unwrap();
    assert_eq!(
        s.admission(0, 2, 2, Admission::Rejected),
        Ok(Change::Unchanged)
    );
    assert_eq!(
        s.admission(0, 3, 3, Admission::NoEffect),
        Ok(Change::Unchanged)
    );
    assert_eq!(s.view().unwrap(), before);
    assert_eq!(
        s.admission(
            0,
            4,
            4,
            Admission::Applied(Transition::Reboot { new_boot: 0 })
        ),
        Err(Error::InvalidTransition)
    );
    assert_eq!(s.view().unwrap(), before);
    s.uncertain_failure();
    assert_eq!(s.view(), Err(Error::Poisoned));
    assert!(matches!(s.facts(), Err(Error::Poisoned)));
}
#[test]
fn only_accepted_current_boot_commands_can_acknowledge() {
    let mut s = state();
    ready(&mut s, 1);
    let before = s.view().unwrap();
    assert_eq!(
        s.response(
            0,
            2,
            2,
            &Response::CommandAcknowledged {
                id: id(41),
                result: CommandResult::Completed
            }
        ),
        Err(Error::UnknownCommand)
    );
    assert_eq!(s.view().unwrap(), before);
    command(&mut s, 41, ApplicationCommand::Stop, 4);
    assert_eq!(
        s.response(
            0,
            3,
            5,
            &Response::CommandAcknowledged {
                id: id(41),
                result: CommandResult::Completed
            }
        ),
        Err(Error::InvalidTime)
    );
    assert!(s.commands.contains_key(&id(41)));
    ack(&mut s, 41, CommandResult::Completed, 5, 5);
    assert_eq!(
        s.response(
            0,
            6,
            6,
            &Response::CommandAcknowledged {
                id: id(41),
                result: CommandResult::Completed
            }
        ),
        Err(Error::UnknownCommand)
    );
    s.admission(
        0,
        7,
        7,
        Admission::Applied(Transition::Reboot { new_boot: 1 }),
    )
    .unwrap();
    assert_eq!(
        s.response(
            0,
            6,
            8,
            &Response::CommandAcknowledged {
                id: id(41),
                result: CommandResult::Completed
            }
        ),
        Ok(Change::Retired)
    );
    assert_eq!(
        s.response(2, 8, 8, &Response::Ready),
        Err(Error::FutureBoot)
    );
}
#[test]
fn copies_retain_current_pending_state_without_claiming_whole_world_snapshot() {
    let mut s = state();
    ready(&mut s, 1);
    command(&mut s, 5, ApplicationCommand::Stop, 2);
    let mut copy = s.clone();
    ack(&mut s, 5, CommandResult::Completed, 3, 3);
    assert_eq!(
        copy.view().unwrap().application.condition,
        ApplicationCondition::Running
    );
    ack(&mut copy, 5, CommandResult::Completed, 3, 3);
    assert_eq!(copy.view().unwrap(), s.view().unwrap());
}

#[test]
fn confirmed_repeated_transitions_refresh_time_but_no_effect_does_not() {
    let mut s = state();
    s.admission(
        0,
        4,
        4,
        Admission::Applied(Transition::Reboot { new_boot: 1 }),
    )
    .unwrap();
    assert_eq!(s.view().unwrap().condition_since, 4);
    assert_eq!(s.view().unwrap().application.changed_at, 4);
    s.admission(1, 5, 5, Admission::NoEffect).unwrap();
    assert_eq!(s.view().unwrap().condition_since, 4);
    s.admission(1, 6, 6, Admission::Applied(Transition::Crash))
        .unwrap();
    s.admission(1, 7, 7, Admission::Applied(Transition::Crash))
        .unwrap();
    assert_eq!(s.view().unwrap().condition_since, 7);
    assert_eq!(s.view().unwrap().application.changed_at, 7);
    let mut s = state();
    ready(&mut s, 1);
    command(&mut s, 1, ApplicationCommand::Resume, 2);
    ack(&mut s, 1, CommandResult::Completed, 3, 3);
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Running
    );
    assert_eq!(s.view().unwrap().application.changed_at, 3);
    command(&mut s, 2, ApplicationCommand::Resume, 4);
    ack(&mut s, 2, CommandResult::Completed, 5, 5);
    assert_eq!(s.view().unwrap().application.changed_at, 5);
}
#[test]
fn service_correlation_is_consumed_after_settlement_then_terminal() {
    use super::super::{
        service::{Adapter, Plan, Service},
        OperationError, Target,
    };
    struct Owner {
        state: State,
        next: u32,
    }
    impl Adapter for Owner {
        fn resolve(&self, target: &Target) -> Result<Facts, OperationError> {
            let (Target::Machine(name) | Target::Service(name) | Target::Key(name)) = target;
            if name != "peer" {
                return Err(OperationError::invalid("unknown target"));
            }
            self.state
                .facts()
                .map_err(|_| OperationError::unsupported())
        }
        fn command_id(
            &mut self,
            _: &str,
            _: u64,
            _: u32,
            _: ApplicationCommand,
        ) -> Result<NonZeroU32, String> {
            self.next += 1;
            Ok(id(self.next))
        }
        fn command_owner(
            &self,
            _: &str,
            boot: u64,
            id: NonZeroU32,
        ) -> Result<CommandOwner, String> {
            self.state
                .command_owner(boot, id)
                .map_err(|e| format!("tracker: {e:?}"))
        }
        fn inspect(
            &self,
            _: &[String],
            _: &[super::super::triggers::Record],
        ) -> Result<serde_json::Value, OperationError> {
            Err(OperationError::unsupported())
        }
    }
    let mut owner = Owner {
        state: state(),
        next: 40,
    };
    ready(&mut owner.state, 1);
    let mut service = Service::default();
    let control = crate::pv::SimulationControlId {
        session: id(1),
        request: id(1),
    };
    let plans = service
        .admit(
            &mut owner,
            "caller".into(),
            control,
            2,
            2,
            br#"{"version":1,"id":1,"op":"stop","target":{"machine":"peer"}}"#,
        )
        .unwrap();
    let Plan::Effect {
        fault,
        command: Some(command),
        ..
    } = plans[0]
    else {
        panic!()
    };
    // Queue reservation has not been accepted by the machine yet.
    assert!(matches!(
        owner.state.command_owner(0, command),
        Err(Error::UnknownCommand)
    ));
    owner
        .state
        .command_admitted(
            0,
            command,
            ApplicationCommand::Stop,
            Origin::Service { fault },
            2,
            2,
        )
        .unwrap();
    let plans = service
        .acknowledged(&owner, "peer", 0, command, CommandResult::Completed, 3, 10)
        .unwrap();
    assert!(plans.iter().any(|p|matches!(p,Plan::Settled(s) if s.status==super::super::service::SettlementStatus::Applied)));
    owner
        .state
        .response(
            0,
            3,
            10,
            &Response::CommandAcknowledged {
                id: command,
                result: CommandResult::Completed,
            },
        )
        .unwrap();
    assert!(matches!(
        owner.state.command_owner(0, command),
        Err(Error::UnknownCommand)
    ));
    owner
        .state
        .terminal(0, 4, 10, ExitReason::Shutdown)
        .unwrap();
    assert_eq!(
        owner.state.view().unwrap().application.condition,
        ApplicationCondition::Stopped
    );
    assert_eq!(owner.state.view().unwrap().first_terminal.unwrap().at, 4);
}

#[test]
fn physical_crash_updates_visible_condition_without_inventing_logical_down() {
    let mut s = state();
    ready(&mut s, 1);
    s.terminal(0, 4, 10, ExitReason::Crashed).unwrap();
    let view = s.view().unwrap();
    assert_eq!(view.machine, MachineCondition::Down);
    assert_eq!(view.condition_since, 4);
    assert_eq!(
        view.application.condition,
        ApplicationCondition::Unavailable
    );
    assert_eq!(s.facts().unwrap().vm, Vm::Up);
    assert_eq!(s.facts().unwrap().process, Process::Running);
    assert!(s.facts().unwrap().physically_ended);
    s.admission(0, 11, 11, Admission::Applied(Transition::Crash))
        .unwrap();
    assert_eq!(s.facts().unwrap().vm, Vm::Down);
    assert_eq!(s.view().unwrap().condition_since, 11);
    assert_eq!(s.view().unwrap().first_terminal.unwrap().at, 4);
}

#[test]
fn natural_exit_keeps_same_boot_start_eligible_without_faking_commanded_stop() {
    let mut s = state();
    ready(&mut s, 1);
    s.response(0, 7, 10, &Response::ApplicationOutcome { code: 23 })
        .unwrap();
    assert_eq!(s.facts().unwrap().process, Process::Stopped);
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Exited
    );
    assert!(s.view().unwrap().first_terminal.is_none());
    command(&mut s, 7, ApplicationCommand::Start, 11);
    ack(&mut s, 7, CommandResult::Completed, 12, 15);
    assert_eq!(s.facts().unwrap().boot, 0);
    assert_eq!(s.facts().unwrap().process, Process::Running);
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Running
    );
    assert_eq!(s.view().unwrap().application.exit_code, None);
    assert_eq!(
        s.view().unwrap().natural_outcome,
        Some(NaturalOutcome { at: 7, code: 23 })
    );
}

#[test]
fn old_application_acks_do_not_rewrite_outcome_or_fresh_start() {
    let mut s = state();
    ready(&mut s, 0);
    for (id, action) in [
        (1, ApplicationCommand::Stop),
        (2, ApplicationCommand::Pause),
        (3, ApplicationCommand::Resume),
        (4, ApplicationCommand::Start),
    ] {
        command(&mut s, id, action, 1);
    }
    s.response(0, 2, 2, &Response::ApplicationOutcome { code: 23 })
        .unwrap();
    assert!(matches!(
        s.command_owner(0, id(1)).unwrap(),
        CommandOwner::RetiredApplication
    ));
    assert!(matches!(
        s.command_owner(0, id(4)).unwrap(),
        CommandOwner::External
    ));
    assert_eq!(
        s.response(
            0,
            3,
            3,
            &Response::CommandAcknowledged {
                id: id(1),
                result: CommandResult::Completed
            }
        )
        .unwrap(),
        Change::Retired
    );
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Exited
    );
    ack(&mut s, 4, CommandResult::Completed, 4, 4);
    for (id, result) in [(2, CommandResult::Rejected), (3, CommandResult::Completed)] {
        assert_eq!(
            s.response(
                0,
                5,
                5,
                &Response::CommandAcknowledged {
                    id: self::id(id),
                    result
                }
            )
            .unwrap(),
            Change::Retired
        );
    }
    assert_eq!(
        s.view().unwrap().application.condition,
        ApplicationCondition::Running
    );
    assert_eq!(s.view().unwrap().application.changed_at, 4);
    s.response(0, 6, 6, &Response::ApplicationOutcome { code: 7 })
        .unwrap();
    assert_eq!(s.view().unwrap().application.exit_code, Some(7));
    assert!(s.view().unwrap().first_terminal.is_none());
}
