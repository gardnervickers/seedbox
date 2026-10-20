//! Field and tag order is the checkpoint encoding; changing it needs a
//! checkpoint `VERSION` bump.
use super::*;
wire_enum!(ApplicationCondition; units {0=>Starting,1=>Running,2=>Stopped,3=>Paused,4=>Exited,5=>Unavailable} tuples {} records {});
wire_struct!(ApplicationView {
    condition,
    changed_at,
    exit_code
});
wire_struct!(NaturalOutcome { at, code });
wire_struct!(Terminal { at, reason });
wire_enum!(Transition; units {0=>Pause,1=>Resume,2=>Crash} tuples {4=>Execution(v0),5=>ClockRate(v0)} records {3=>Reboot{new_boot}});
wire_enum!(Origin; units {0=>External} tuples {} records {1=>Service{fault}});
wire_struct!(AcceptedCommand {
    action,
    origin,
    accepted_at
});
wire_enum!(Gate; units {0=>Enabled,1=>Paused,2=>Crashed} tuples {} records {});
wire_struct!(State {
    name,
    boot,
    boot_at,
    committed_at,
    gate,
    process,
    ready,
    execution,
    condition_since,
    last_transition,
    application,
    natural_outcome,
    terminal,
    commands,
    retired_commands,
    poisoned
});

impl State {
    pub(crate) fn check_persist(&self, now: u64, name: &str) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        require(
            !self.poisoned
                && self.name == name
                && self.boot_at <= self.committed_at
                && self.committed_at <= now
                && self.condition_since <= now
                && self.application.changed_at <= now,
            "lifecycle state",
        )?;
        require(
            self.natural_outcome.is_none_or(|x| x.at <= now)
                && self.terminal.is_none_or(|x| x.at <= now)
                && self.last_transition.is_none_or(|(_, at)| at <= now),
            "lifecycle time",
        )?;
        for (id, c) in &self.commands {
            require(
                !self.retired_commands.contains_key(id) && c.accepted_at <= now,
                "command correlation",
            )?;
        }
        for c in self.retired_commands.values() {
            require(c.accepted_at <= now, "retired command time")?;
        }
        Ok(())
    }
}
