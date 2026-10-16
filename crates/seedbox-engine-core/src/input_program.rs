//! Ordered external inputs enter the ordinary fault sequence only when due.
use crate::faults::Faults;
use seedbox_control::{Action, Plan};
pub use seedbox_engine_protocol::input_program::{
    merge_interventions, validate_program, ArmReceipt, BranchAction, BranchReceipt, InputAction,
    InputAdmission, InputRejection, RuleAction, TimedInput,
};
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    inputs: Vec<TimedInput>,
    next: usize,
    admissions: Vec<InputAdmission>,
    failed: bool,
    #[serde(default)]
    rejections: Vec<InputRejection>,
    /// Generated starts that were skipped; their teardowns follow them.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    skipped: std::collections::BTreeSet<String>,
    /// Consumed anchors and reseeds, which allocate no fault IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    branches: Vec<BranchReceipt>,
    /// Consumed trigger inputs and the trigger IDs they armed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    armed: Vec<ArmReceipt>,
}

/// What admitting one due input did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admitted {
    /// A fault was admitted, a trigger armed, or the input was rejected
    /// or skipped.
    Fault,
    /// The anchor phase at this time; a capture may follow.
    Anchor { key: String },
    /// Every future draw changes from here, before ordinary inputs due now.
    Reseed { seed: u64, identity: String },
    /// A choice override, active from now until a request consumes it.
    Choice {
        key: String,
        choice: seedbox_engine_protocol::input_program::ChoiceOverride,
    },
}

/// World conditions that gate generated inputs at their boundary.
#[derive(Clone, Copy, Debug)]
pub struct Gate {
    /// Every initial application has reported ready.
    pub ready: bool,
    /// Generated starts due before this time are skipped.
    pub quiet_until: u64,
}
impl Program {
    pub fn new(inputs: Vec<TimedInput>, duration_ns: u64, from: u64) -> Result<Self, String> {
        validate_program(&inputs, duration_ns)?;
        if inputs.first().is_some_and(|i| i.at_ns < from) {
            return Err("input program begins before the application release".into());
        }
        Ok(Self {
            inputs,
            ..Self::default()
        })
    }
    pub fn next_time(&self) -> u64 {
        self.inputs.get(self.next).map_or(u64::MAX, |i| i.at_ns)
    }
    /// The next input is an anchor or reseed.
    pub fn next_is_branch(&self) -> bool {
        self.inputs
            .get(self.next)
            .is_some_and(|i| matches!(i.action, InputAction::Branch(_)))
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.inputs.iter().any(|input| input.key == key)
    }
    pub fn rejections(&self) -> &[InputRejection] {
        &self.rejections
    }
    pub fn branches(&self) -> &[BranchReceipt] {
        &self.branches
    }
    pub fn armed(&self) -> &[ArmReceipt] {
        &self.armed
    }
    /// An edited checkpoint definition replaces only unapplied actions. The
    /// consumed prefix belongs to the saved world and cannot be undone.
    pub fn replace_pending(&mut self, inputs: Vec<TimedInput>) -> Result<(), String> {
        self.validate()?;
        validate_program(&inputs, u64::MAX)?;
        let prefix = &self.inputs[..self.next];
        let mut replaced = prefix.to_vec();
        for input in inputs {
            if let Some(applied) = prefix.iter().find(|i| i.key == input.key) {
                if applied != &input {
                    return Err(format!(
                        "cannot change applied override {} in a checkpoint",
                        input.key
                    ));
                }
            } else {
                replaced.push(input);
            }
        }
        validate_program(&replaced, u64::MAX)?;
        self.inputs = replaced;
        Ok(())
    }
    pub fn commit_live(
        &mut self,
        faults: &mut Faults,
        key: String,
        plan: Plan,
        at: u64,
    ) -> Result<(), String> {
        self.validate()?;
        if self.inputs.iter().any(|input| input.key == key) {
            return Err(format!("duplicate input identity {key}"));
        }
        let input = TimedInput {
            key: key.clone(),
            at_ns: plan.at_ns,
            action: plan.action.clone().into(),
            timeout_ns: plan
                .deadline_ns
                .checked_sub(plan.at_ns)
                .ok_or("invalid input deadline")?,
        };
        if self
            .inputs
            .get(self.next)
            .is_some_and(|pending| pending.at_ns <= input.at_ns)
        {
            return Err("pending override must run before live admission".into());
        }
        let mut inputs = self.inputs.clone();
        inputs.insert(self.next, input);
        validate_program(&inputs, u64::MAX)?;
        faults.admit_exact(plan.clone(), at)?;
        self.inputs = inputs;
        self.admissions.push(InputAdmission { key, plan });
        self.next += 1;
        Ok(())
    }
    pub fn admissions(&self) -> &[InputAdmission] {
        &self.admissions
    }
    pub fn key_for(&self, id: u32) -> Option<&str> {
        self.admissions
            .iter()
            .find(|a| a.plan.id == id)
            .map(|a| a.key.as_str())
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.failed {
            return Err("input program admission previously failed".into());
        }
        validate_program(&self.inputs, u64::MAX)?;
        if self.next
            != self.admissions.len()
                + self.rejections.len()
                + self.branches.len()
                + self.armed.len()
            || self.next > self.inputs.len()
        {
            return Err("input program cursor differs from its admission history".into());
        }
        let mut admitted = self.admissions.iter().peekable();
        let mut rejected = self.rejections.iter().peekable();
        let mut branched = self.branches.iter();
        let mut armed = self.armed.iter();
        let mut previous_trigger = 0;
        let mut previous = 0;
        for input in self.inputs.iter().take(self.next) {
            if let InputAction::Branch(action) = &input.action {
                let receipt = branched.next().ok_or("missing branch input receipt")?;
                if receipt.key != input.key
                    || receipt.at_ns != input.at_ns
                    || &receipt.action != action
                {
                    return Err("branch input receipt differs from its source".into());
                }
                continue;
            }
            if rejected.peek().is_some_and(|r| r.key == input.key) {
                let r = rejected.next().ok_or("missing input rejection evidence")?;
                if r.at_ns != input.at_ns || r.reason.is_empty() {
                    return Err("invalid input rejection evidence".into());
                }
                continue;
            }
            if let InputAction::Rule(_) = &input.action {
                let receipt = armed.next().ok_or("missing rule input receipt")?;
                receipt.validate(input, &mut previous_trigger)?;
                continue;
            }
            let row = admitted.next().ok_or("missing input admission evidence")?;
            if input.key != row.key
                || input.at_ns != row.plan.at_ns
                || input.action.fault() != Some(&row.plan.action)
                || row.plan.id <= previous
                || Some(row.plan.deadline_ns) != input.at_ns.checked_add(input.timeout_ns)
            {
                return Err("input program admission differs from its source".into());
            }
            previous = row.plan.id;
        }
        Ok(())
    }
    /// Commit one input. Callers drain its due effects before the next input at
    /// the same time, without guest work in between, so dependencies see state.
    pub fn admit_due(
        &mut self,
        faults: &mut Faults,
        triggers: &mut crate::triggers::Triggers,
        at: u64,
        gate: Gate,
        validate_action: &dyn Fn(&Action) -> Result<(), String>,
    ) -> Result<Admitted, String> {
        self.validate()?;
        let input = self
            .inputs
            .get(self.next)
            .ok_or("no input program entry due")?
            .clone();
        if input.at_ns != at {
            return Err("wrong input program boundary".into());
        }
        let key = input.key.clone();
        let action = match &input.action {
            InputAction::Fault(action) => action.clone(),
            InputAction::Branch(branch) => {
                self.branches.push(BranchReceipt {
                    key: input.key.clone(),
                    at_ns: at,
                    action: branch.clone(),
                });
                self.next += 1;
                return Ok(match branch {
                    BranchAction::Anchor {} => Admitted::Anchor { key },
                    BranchAction::Reseed { seed, identity } => Admitted::Reseed {
                        seed: *seed,
                        identity: identity.clone(),
                    },
                });
            }
            InputAction::Rule(RuleAction::Trigger(spec)) => {
                // A malformed rule is an error; a full budget is a rejection.
                let shape = spec
                    .when
                    .machines()
                    .into_iter()
                    .find(|m| !faults.names().iter().any(|n| n == m))
                    .map_or(Ok(()), |m| Err(format!("unknown machine {m}")))
                    .and_then(|()| faults.validate_action_shape(&spec.then));
                if let Err(e) = shape {
                    self.failed = true;
                    return Err(format!("input {key}: {e}"));
                }
                match triggers.arm_typed(&key, spec, at) {
                    Ok(trigger_id) => self.armed.push(ArmReceipt {
                        key,
                        at_ns: at,
                        trigger_id: Some(trigger_id),
                    }),
                    Err(reason) => self.rejections.push(InputRejection {
                        key,
                        at_ns: at,
                        reason,
                    }),
                }
                self.next += 1;
                return Ok(Admitted::Fault);
            }
            InputAction::Rule(RuleAction::Choice(choice)) => {
                if !faults.names().iter().any(|n| n == &choice.machine) {
                    self.failed = true;
                    return Err(format!("input {key}: unknown machine {}", choice.machine));
                }
                self.armed.push(ArmReceipt {
                    key: key.clone(),
                    at_ns: at,
                    trigger_id: None,
                });
                self.next += 1;
                return Ok(Admitted::Choice {
                    key,
                    choice: choice.clone(),
                });
            }
        };
        let result = (|| {
            // Malformed targets/parameters remain errors. Current world state
            // can diverge after an edit; record its deterministic rejection.
            faults.validate_action_shape(&action)?;
            if let Some(reason) = self.skip(&input, at, gate) {
                use seedbox_engine_protocol::input_program::GENERATED_END_SUFFIX;
                if !input.key.ends_with(GENERATED_END_SUFFIX) {
                    self.skipped.insert(input.key.clone());
                }
                self.rejections.push(InputRejection {
                    key: input.key.clone(),
                    at_ns: at,
                    reason: reason.into(),
                });
                self.next += 1;
                return Ok(());
            }
            let runtime =
                validate_action(&action).and_then(|()| validate_override_state(faults, &action));
            if let Err(reason) = runtime {
                self.rejections.push(InputRejection {
                    key: input.key.clone(),
                    at_ns: at,
                    reason,
                });
                self.next += 1;
                return Ok(());
            }
            let plan = faults.propose(
                Plan {
                    id: 0,
                    at_ns: at,
                    deadline_ns: at
                        .checked_add(input.timeout_ns)
                        .ok_or("input deadline overflow")?,
                    action,
                },
                at,
            )?;
            faults.admit_exact(plan.clone(), at)?;
            self.admissions.push(InputAdmission {
                key: input.key.clone(),
                plan,
            });
            self.next += 1;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
            .map(|()| Admitted::Fault)
            .map_err(|e: String| format!("input {key}: {e}"))
    }
    /// Why a generated input is skipped here, if it is. Authored inputs and
    /// teardowns of admitted starts always run.
    fn skip(&self, input: &TimedInput, at: u64, gate: Gate) -> Option<&'static str> {
        use seedbox_engine_protocol::input_program::{GENERATED_END_SUFFIX, GENERATED_PREFIX};
        if !input.key.starts_with(GENERATED_PREFIX) {
            return None;
        }
        match input.key.strip_suffix(GENERATED_END_SUFFIX) {
            Some(start) => self
                .skipped
                .contains(start)
                .then_some("skipped: its generated fault was skipped"),
            None if !gate.ready => Some("skipped: generated before the applications were ready"),
            None if at < gate.quiet_until => Some("skipped: generated during a quiet period"),
            None => None,
        }
    }
}

/// State-dependent rejection shared by timed and live external actions.
pub(crate) fn validate_override_state(faults: &Faults, action: &Action) -> Result<(), String> {
    faults
        .validate_admission_state(action)
        .map_err(|e| e.message().to_owned())?;
    let handle = match action {
        Action::Partition { handle, .. }
        | Action::Shape { handle, .. }
        | Action::DropConnection { handle, .. }
        | Action::ClockSkew { handle, .. }
        | Action::ClockDrift { handle, .. }
        | Action::Throttle { handle, .. }
        | Action::DiskError { handle, .. } => Some(handle),
        _ => None,
    };
    if handle.is_some_and(|handle| faults.active_handles().contains(handle)) {
        Err("duplicate active handle".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn input(key: &str, at_ns: u64, action: Action) -> TimedInput {
        TimedInput {
            key: key.into(),
            at_ns,
            action: action.into(),
            timeout_ns: 1_000,
        }
    }
    fn partition(handle: &str) -> Action {
        Action::Partition {
            handle: handle.into(),
            left: vec!["a".into()],
            right: vec!["b".into()],
        }
    }
    fn heal(handle: &str) -> Action {
        Action::Heal {
            handle: handle.into(),
        }
    }

    #[test]
    fn generated_starts_wait_for_readiness_and_quiet_and_take_their_teardowns() {
        let mut faults = Faults::new(Vec::new(), vec!["a".into(), "b".into()]).unwrap();
        let mut triggers = crate::triggers::Triggers::default();
        let mut program = Program::new(
            vec![
                input("gen-0", 5, partition("gen-0")),
                input("mine", 5, partition("mine")),
                input("gen-0-end", 6, heal("gen-0")),
                input("gen-1", 20, partition("gen-1")),
                input("gen-1-end", 21, heal("gen-1")),
                input("gen-2", 30, partition("gen-2")),
            ],
            u64::MAX,
            0,
        )
        .unwrap();
        let ok = |_: &Action| Ok(());
        let not_ready = Gate {
            ready: false,
            quiet_until: 0,
        };
        let quiet = Gate {
            ready: true,
            quiet_until: 25,
        };
        let open = Gate {
            ready: true,
            quiet_until: 25,
        };
        for (at, gate) in [
            (5, not_ready),
            (5, not_ready),
            (6, not_ready),
            (20, quiet),
            (21, quiet),
            (30, open),
        ] {
            program
                .admit_due(&mut faults, &mut triggers, at, gate, &ok)
                .unwrap();
        }
        let skipped: Vec<(&str, &str)> = program
            .rejections()
            .iter()
            .map(|r| (r.key.as_str(), r.reason.as_str()))
            .collect();
        assert_eq!(
            skipped,
            [
                (
                    "gen-0",
                    "skipped: generated before the applications were ready"
                ),
                ("gen-0-end", "skipped: its generated fault was skipped"),
                ("gen-1", "skipped: generated during a quiet period"),
                ("gen-1-end", "skipped: its generated fault was skipped"),
            ]
        );
        // Authored inputs ignore the gate; generated ones run once it opens.
        let admitted: Vec<&str> = program
            .admissions()
            .iter()
            .map(|a| a.key.as_str())
            .collect();
        assert_eq!(admitted, ["mine", "gen-2"]);
        let saved: Program =
            serde_json::from_value(serde_json::to_value(&program).unwrap()).unwrap();
        assert_eq!(saved.skipped, program.skipped);
    }
}
