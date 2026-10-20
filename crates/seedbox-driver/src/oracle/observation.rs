//! The Driver-owned committed lifecycle record of one machine, read by the
//! private service adapter. The outer owner supplies authoritative logical
//! admissions and ordered committed observations; polling later adopted
//! StopReason::Exited boundaries cannot replace this seam. Scheduling reads its
//! boot, pause gate and execution setting; no fault history, complete Inspect or
//! network projection lives here.
// Boot, pause and execution live only in this one committed record per
// machine, not in mirrors in Entry or control. Accepted run progress
// (StopReason, positions) stays in Entry: it changes within a round, while
// this record changes only at committed boundaries and confirmed effects.
use super::service::{CommandKind, CommandOwner, Facts, Process, Vm};
use crate::{
    execution::ExecutionConfig,
    machine::ExitReason,
    pv::{ApplicationCommand, CommandResult, Response},
};
use std::{collections::BTreeMap, num::NonZeroU32};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApplicationCondition {
    Starting,
    Running,
    Stopped,
    Paused,
    Exited,
    Unavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MachineCondition {
    Starting,
    Ready,
    Paused,
    Down,
    Exited,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApplicationView {
    pub condition: ApplicationCondition,
    pub changed_at: u64,
    pub exit_code: Option<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NaturalOutcome {
    pub at: u64,
    pub code: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Terminal {
    pub at: u64,
    pub reason: ExitReason,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    Pause,
    Resume,
    Crash,
    Reboot { new_boot: u64 },
    Execution(ExecutionConfig),
    ClockRate(crate::clock::ClockRate),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    External,
    Service { fault: u32 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AcceptedCommand {
    pub action: CommandKind,
    pub origin: Origin,
    pub accepted_at: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Changed,
    Unchanged,
    Retired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Poisoned,
    InvalidTime,
    FutureBoot,
    InvalidTransition,
    UnknownCommand,
    DuplicateCommand,
    DuplicateReady,
    UnavailableAdmission,
}

/// Current projection only; the outer owner retains historical publications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct View {
    pub name: String,
    pub boot: u64,
    pub machine: MachineCondition,
    pub condition_since: u64,
    pub last_transition: Option<(Transition, u64)>,
    pub application: ApplicationView,
    pub natural_outcome: Option<NaturalOutcome>,
    pub first_terminal: Option<Terminal>,
    pub execution: ExecutionConfig,
    pub ready: bool,
}
/// Logical lifecycle gate of the current boot. Pause is the driver's withholding
/// of positive budget; Crashed is an applied logical Crash. A confirmed physical
/// crash (`ExitReason::Crashed` in `terminal`) makes the boot Down without
/// changing this gate, so a machine paused when it crashed stays paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Enabled,
    Paused,
    Crashed,
}
#[derive(Clone)]
pub(crate) struct State {
    name: String,
    boot: u64,
    boot_at: u64,
    committed_at: u64,
    gate: Gate,
    process: Process,
    ready: bool,
    execution: ExecutionConfig,
    condition_since: u64,
    last_transition: Option<(Transition, u64)>,
    application: ApplicationView,
    natural_outcome: Option<NaturalOutcome>,
    terminal: Option<Terminal>,
    // Only accepted outstanding work, not a duplicate command history. Future
    // unbound reservations remain in the outer shared allocator across reboot.
    commands: BTreeMap<NonZeroU32, AcceptedCommand>,
    // Settled old-application requests can still produce one legitimate late Ack.
    retired_commands: BTreeMap<NonZeroU32, AcceptedCommand>,
    poisoned: bool,
}
impl State {
    pub(crate) fn new(name: String, boot: u64, at: u64, execution: ExecutionConfig) -> Self {
        Self {
            name,
            boot,
            boot_at: at,
            committed_at: at,
            gate: Gate::Enabled,
            process: Process::Running,
            ready: false,
            execution,
            condition_since: at,
            last_transition: None,
            application: ApplicationView {
                condition: ApplicationCondition::Starting,
                changed_at: at,
                exit_code: None,
            },
            natural_outcome: None,
            terminal: None,
            commands: BTreeMap::new(),
            retired_commands: BTreeMap::new(),
            poisoned: false,
        }
    }
    fn check(&self, boot: u64, observed: u64, commit: u64) -> Result<Change, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if observed > commit || commit < self.committed_at {
            return Err(Error::InvalidTime);
        }
        if boot < self.boot {
            return Ok(Change::Retired);
        }
        if boot > self.boot {
            return Err(Error::FutureBoot);
        }
        if observed < self.boot_at {
            return Err(Error::InvalidTime);
        }
        Ok(Change::Unchanged)
    }
    /// The current boot's committed number; outputs of a Reboot admission run
    /// carry the prospective next boot before `admission` commits it.
    pub(crate) fn boot(&self) -> u64 {
        self.boot
    }
    /// Whether the driver withholds positive execution budget. A confirmed
    /// physical crash leaves this unchanged; logical Crash and Reboot clear it.
    pub(crate) fn paused(&self) -> bool {
        self.gate == Gate::Paused
    }
    pub(crate) fn execution(&self) -> ExecutionConfig {
        self.execution
    }
    /// Retained control-policy lifecycle: Down after an applied Crash or a
    /// confirmed physical crash. Shutdown and program exit keep the gate.
    fn vm(&self) -> Vm {
        let crashed = self
            .terminal
            .is_some_and(|terminal| terminal.reason == ExitReason::Crashed);
        match self.gate {
            Gate::Crashed => Vm::Down,
            _ if crashed => Vm::Down,
            Gate::Paused => Vm::Paused,
            Gate::Enabled => Vm::Up,
        }
    }
    /// Display condition, derived from the gate, the first terminal and Ready.
    fn condition(&self) -> MachineCondition {
        if self.vm() == Vm::Down {
            MachineCondition::Down
        } else if self.terminal.is_some() {
            MachineCondition::Exited
        } else if self.gate == Gate::Paused {
            MachineCondition::Paused
        } else if self.ready {
            MachineCondition::Ready
        } else {
            MachineCondition::Starting
        }
    }
    /// The displayed condition changed or was confirmed again at `at`, even
    /// when its label is unchanged (reboot-before-ready or repeated Crash).
    fn since(&mut self, at: u64) {
        self.condition_since = at;
    }
    fn application(&mut self, condition: ApplicationCondition, code: Option<u8>, at: u64) {
        // Confirmed process effects update changed_at even if an external
        // idempotent command leaves the visible condition unchanged.
        self.application = ApplicationView {
            condition,
            changed_at: at,
            exit_code: code,
        };
    }
    pub(crate) fn view(&self) -> Result<View, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        Ok(View {
            name: self.name.clone(),
            boot: self.boot,
            machine: self.condition(),
            condition_since: self.condition_since,
            last_transition: self.last_transition,
            application: self.application,
            natural_outcome: self.natural_outcome,
            first_terminal: self.terminal,
            execution: self.execution,
            ready: self.ready,
        })
    }
    pub(crate) fn facts(&self) -> Result<Facts, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        Ok(Facts {
            name: self.name.clone(),
            boot: self.boot,
            vm: self.vm(),
            process: self.process,
            physically_ended: self.terminal.is_some(),
            ready: self.ready,
        })
    }
    /// All actual logical effects, including external controls, must be supplied.
    /// The transition is a confirmed logical effect, not proof that native RAM
    /// changed: a logical Crash still applies to an exited machine whose
    /// ProgramExit is sticky.
    pub(crate) fn admission(
        &mut self,
        boot: u64,
        at: u64,
        commit: u64,
        transition: Transition,
    ) -> Result<Change, Error> {
        if self.check(boot, at, commit)? == Change::Retired {
            return Ok(Change::Retired);
        }
        match transition {
            Transition::Pause if self.vm() != Vm::Up => return Err(Error::InvalidTransition),
            Transition::Resume if self.vm() != Vm::Paused => {
                return Err(Error::InvalidTransition);
            }
            Transition::Reboot { new_boot } if new_boot <= self.boot => {
                return Err(Error::InvalidTransition);
            }
            _ => {}
        }
        match transition {
            Transition::Pause => {
                self.gate = Gate::Paused;
                self.since(at);
            }
            Transition::Resume => {
                self.gate = Gate::Enabled;
                self.since(at);
            }
            Transition::Crash => {
                self.gate = Gate::Crashed;
                self.process = Process::Running;
                self.ready = false;
                self.commands.clear();
                self.retired_commands.clear();
                self.since(at);
                self.application(ApplicationCondition::Unavailable, None, at);
            }
            Transition::Reboot { new_boot } => {
                self.boot = new_boot;
                self.boot_at = at;
                self.gate = Gate::Enabled;
                self.process = Process::Running;
                self.ready = false;
                self.commands.clear();
                self.retired_commands.clear();
                self.terminal = None;
                self.natural_outcome = None;
                self.since(at);
                self.application = ApplicationView {
                    condition: ApplicationCondition::Starting,
                    changed_at: at,
                    exit_code: None,
                };
            }
            Transition::Execution(config) => self.execution = config,
            Transition::ClockRate(_) => {}
        }
        self.last_transition = Some((transition, at));
        self.committed_at = commit;
        Ok(Change::Changed)
    }
    /// IDs come from the outer shared allocator. Admission order may differ
    /// from ID order; reservations alone must never call this method.
    pub(crate) fn command_admitted(
        &mut self,
        boot: u64,
        id: NonZeroU32,
        action: CommandKind,
        origin: Origin,
        at: u64,
        commit: u64,
    ) -> Result<Change, Error> {
        if self.check(boot, at, commit)? == Change::Retired {
            return Ok(Change::Retired);
        }
        if self.vm() == Vm::Down || self.terminal.is_some() {
            return Err(Error::UnavailableAdmission);
        }
        if self.commands.contains_key(&id) || self.retired_commands.contains_key(&id) {
            return Err(Error::DuplicateCommand);
        }
        self.commands.insert(
            id,
            AcceptedCommand {
                action,
                origin,
                accepted_at: at,
            },
        );
        self.committed_at = commit;
        Ok(Change::Changed)
    }
    pub(crate) fn command_owner(&self, boot: u64, id: NonZeroU32) -> Result<CommandOwner, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if boot < self.boot
            || (boot == self.boot && (self.vm() == Vm::Down || self.terminal.is_some()))
        {
            return Ok(CommandOwner::RetiredBoot);
        }
        if boot > self.boot {
            return Err(Error::FutureBoot);
        }
        if self.retired_commands.contains_key(&id) {
            return Ok(CommandOwner::RetiredApplication);
        }
        let command = self.commands.get(&id).ok_or(Error::UnknownCommand)?;
        Ok(match command.origin {
            Origin::External => CommandOwner::External,
            Origin::Service { fault } => CommandOwner::Service {
                fault,
                action: command.action,
            },
        })
    }
    /// Ordered typed responses carry occurrence time separately from publication.
    /// Command actions are joined to accepted admissions, never guessed from Ack.
    /// The outer owner must settle Service::acknowledged using command_owner
    /// BEFORE folding that acknowledgment here (which consumes correlation).
    /// Likewise fold every final typed response before terminal() retires a boot.
    pub(crate) fn response(
        &mut self,
        boot: u64,
        observed: u64,
        commit: u64,
        response: &Response,
    ) -> Result<Change, Error> {
        if self.check(boot, observed, commit)? == Change::Retired {
            return Ok(Change::Retired);
        }
        if self.vm() == Vm::Down || self.terminal.is_some() {
            return Ok(Change::Retired);
        }
        let change = match response {
            Response::Ready => {
                if self.ready {
                    return Err(Error::DuplicateReady);
                }
                self.ready = true;
                self.application(ApplicationCondition::Running, None, observed);
                // While paused, Ready shows only after Resume.
                if self.gate != Gate::Paused {
                    self.since(observed);
                }
                Change::Changed
            }
            Response::ApplicationOutcome { code } => {
                self.natural_outcome = Some(NaturalOutcome {
                    at: observed,
                    code: *code,
                });
                self.application(ApplicationCondition::Exited, Some(*code), observed);
                // The live supervisor can start another application. Preserve
                // the actual exit observation instead of fabricating Stop.
                self.process = Process::Stopped;
                let ended: Vec<_> = self
                    .commands
                    .iter()
                    .filter(|(_, command)| {
                        // Kernel clock steps outlive the application group.
                        matches!(command.action, CommandKind::Application(action)
                            if action != ApplicationCommand::Start)
                    })
                    .map(|(&id, _)| id)
                    .collect();
                for id in ended {
                    self.retired_commands
                        .insert(id, self.commands.remove(&id).unwrap());
                }
                Change::Changed
            }
            Response::CommandAcknowledged { id, result } => {
                if self.retired_commands.remove(id).is_some() {
                    self.committed_at = commit;
                    return Ok(Change::Retired);
                }
                let command = *self.commands.get(id).ok_or(Error::UnknownCommand)?;
                if observed < command.accepted_at {
                    return Err(Error::InvalidTime);
                }
                self.commands.remove(id);
                let CommandKind::Application(action) = command.action else {
                    // A realtime step changes no lifecycle or process state.
                    self.committed_at = commit;
                    return Ok(Change::Unchanged);
                };
                if *result == CommandResult::Rejected {
                    Change::Unchanged
                } else {
                    let (policy, view) = match action {
                        ApplicationCommand::Start | ApplicationCommand::Resume => {
                            (Process::Running, ApplicationCondition::Running)
                        }
                        ApplicationCommand::Stop => {
                            (Process::Stopped, ApplicationCondition::Stopped)
                        }
                        ApplicationCommand::Pause => {
                            (Process::Frozen, ApplicationCondition::Paused)
                        }
                    };
                    self.process = policy;
                    self.application(view, None, observed);
                    Change::Changed
                }
            }
            _ => Change::Unchanged,
        };
        self.committed_at = commit;
        Ok(change)
    }
    /// Supply the first actual physical terminal transition, not a later adopted
    /// Exited.reached boundary. Repeated observations cannot rewrite occurrence.
    pub(crate) fn terminal(
        &mut self,
        boot: u64,
        observed: u64,
        commit: u64,
        reason: ExitReason,
    ) -> Result<Change, Error> {
        if self.check(boot, observed, commit)? == Change::Retired {
            return Ok(Change::Retired);
        }
        self.committed_at = commit;
        if self.terminal.is_some() {
            return Ok(Change::Unchanged);
        }
        // A logically crashed boot already shows Down from its Crash.
        if self.vm() != Vm::Down {
            self.since(observed);
        }
        // A confirmed crash is the same lifecycle fact as an applied Crash:
        // the boot is down (see `vm`), Reboot is eligible and another Crash is
        // not. Shutdown and program exit keep the gate and still need Crash.
        self.terminal = Some(Terminal {
            at: observed,
            reason,
        });
        self.ready = false;
        self.commands.clear();
        self.retired_commands.clear();
        if !matches!(
            self.application.condition,
            ApplicationCondition::Exited | ApplicationCondition::Stopped
        ) {
            self.application(ApplicationCondition::Unavailable, None, observed);
        }
        Ok(Change::Changed)
    }
    /// Unknown partial effects cannot leave a trustworthy read projection.
    pub(crate) fn uncertain_failure(&mut self) {
        self.poisoned = true;
    }
}
#[cfg(test)]
mod tests;

mod persist;
