//! Ordered external inputs enter the ordinary fault sequence only when due.
use crate::faults::Faults;
use seedbox_control::{Action, Plan};
pub use seedbox_engine_protocol::input_program::{
    merge_interventions, validate_program, InputAdmission, InputRejection, TimedInput,
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
    pub fn contains_key(&self, key: &str) -> bool {
        self.inputs.iter().any(|input| input.key == key)
    }
    pub fn rejections(&self) -> &[InputRejection] {
        &self.rejections
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
            action: plan.action.clone(),
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
        if self.next != self.admissions.len() + self.rejections.len()
            || self.next > self.inputs.len()
        {
            return Err("input program cursor differs from its admission history".into());
        }
        let mut admitted = self.admissions.iter().peekable();
        let mut rejected = self.rejections.iter().peekable();
        let mut previous = 0;
        for input in self.inputs.iter().take(self.next) {
            if rejected.peek().is_some_and(|r| r.key == input.key) {
                let r = rejected.next().ok_or("missing input rejection evidence")?;
                if r.at_ns != input.at_ns || r.reason.is_empty() {
                    return Err("invalid input rejection evidence".into());
                }
                continue;
            }
            let row = admitted.next().ok_or("missing input admission evidence")?;
            if input.key != row.key
                || input.at_ns != row.plan.at_ns
                || input.action != row.plan.action
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
        at: u64,
        validate_action: &dyn Fn(&Action) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate()?;
        let input = self
            .inputs
            .get(self.next)
            .ok_or("no input program entry due")?
            .clone();
        if input.at_ns != at {
            return Err("wrong input program boundary".into());
        }
        let result = (|| {
            // Malformed targets/parameters remain errors. Current world state
            // can diverge after an edit; record its deterministic rejection.
            faults.validate_action_shape(&input.action)?;
            let runtime = validate_action(&input.action)
                .and_then(|()| validate_override_state(faults, &input.action));
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
                    action: input.action,
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
        result.map_err(|e: String| format!("input {}: {e}", input.key))
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
        | Action::Throttle { handle, .. } => Some(handle),
        _ => None,
    };
    if handle.is_some_and(|handle| faults.active_handles().contains(handle)) {
        Err("duplicate active handle".into())
    } else {
        Ok(())
    }
}
