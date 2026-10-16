//! Authored external inputs and their runtime-assigned admission identities.
use seedbox_control::{Action, Plan, PROCESS_EFFECT_TIMEOUT_NS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
pub const MAX_INPUTS: usize = 4096;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimedInput {
    pub key: String,
    pub at_ns: u64,
    pub action: InputAction,
    #[serde(
        default = "default_timeout_ns",
        skip_serializing_if = "is_default_timeout"
    )]
    pub timeout_ns: u64,
}
/// What an input does: an ordinary fault action, or a branch input that
/// consumes no fault ID. Both share the `action`-tagged JSON form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputAction {
    Fault(Action),
    Branch(BranchAction),
}
impl From<Action> for InputAction {
    fn from(action: Action) -> Self {
        Self::Fault(action)
    }
}
impl InputAction {
    pub fn fault(&self) -> Option<&Action> {
        match self {
            Self::Fault(action) => Some(action),
            Self::Branch(_) => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fault(action) => action.name(),
            Self::Branch(BranchAction::Anchor {}) => "anchor",
            Self::Branch(BranchAction::Reseed { .. }) => "reseed",
        }
    }
}
/// Branch inputs. An anchor establishes the timed checkpoint phase at its
/// time, before ordinary actions due then; a capture there is an optional
/// cache of the execution. A reseed at the same time, after its anchor,
/// changes every future draw from then on, identically in a cold run and in a
/// continuation of that capture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum BranchAction {
    Anchor {},
    Reseed { seed: u64, identity: String },
}
/// A consumed branch input: where the program cursor passed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchReceipt {
    pub key: String,
    pub at_ns: u64,
    pub action: BranchAction,
}
/// Keys of inputs sampled from a fault profile. The server reserves this
/// namespace: `gen-<n>` starts a generated fault and `gen-<n>-end` tears it
/// down. Generated starts are skipped before the applications are ready and
/// during quiet periods, and a skipped start's teardown is skipped with it.
pub const GENERATED_PREFIX: &str = "gen-";
pub const GENERATED_END_SUFFIX: &str = "-end";
pub const fn default_timeout_ns() -> u64 {
    PROCESS_EFFECT_TIMEOUT_NS
}
fn is_default_timeout(value: &u64) -> bool {
    *value == default_timeout_ns()
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRejection {
    pub key: String,
    pub at_ns: u64,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputAdmission {
    pub key: String,
    pub plan: Plan,
}
/// Document order breaks equal-time ties. IDs belong to actual admissions,
/// so future inputs reserve none. Action/state checks happen when due.
pub fn validate_program(inputs: &[TimedInput], duration_ns: u64) -> Result<(), String> {
    if inputs.len() > MAX_INPUTS {
        return Err(format!("input program exceeds {MAX_INPUTS} entries"));
    }
    let mut keys = BTreeSet::new();
    let mut previous = 0;
    for input in inputs {
        if input.key.is_empty()
            || input.key.len() > 128
            || !input
                .key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            || !keys.insert(&input.key)
        {
            return Err("input program keys must be unique, nonempty ASCII letters, digits, hyphens or underscores (at most 128 bytes)".into());
        }
        if input.at_ns < previous || input.at_ns >= duration_ns {
            return Err(format!(
                "input {} must be ordered and before the simulated time limit",
                input.key
            ));
        }
        input
            .at_ns
            .checked_add(input.timeout_ns)
            .ok_or_else(|| format!("input {} deadline overflows", input.key))?;
        previous = input.at_ns;
    }
    validate_branches(inputs)
}
/// Branch inputs come first among the inputs due at their time; a reseed
/// follows an anchor at the same time and names a nonempty identity.
fn validate_branches(inputs: &[TimedInput]) -> Result<(), String> {
    let mut ordinary_at = None;
    let mut anchor_at = None;
    for input in inputs {
        match &input.action {
            InputAction::Fault(_) => ordinary_at = Some(input.at_ns),
            InputAction::Branch(branch) => {
                if ordinary_at == Some(input.at_ns) {
                    return Err(format!(
                        "input {}: branch inputs precede the ordinary inputs due at their time",
                        input.key
                    ));
                }
                match branch {
                    BranchAction::Anchor {} => anchor_at = Some(input.at_ns),
                    BranchAction::Reseed { identity, .. } => {
                        if anchor_at != Some(input.at_ns) {
                            return Err(format!(
                                "input {}: a reseed follows an anchor at the same time",
                                input.key
                            ));
                        }
                        if identity.is_empty() || identity.len() > 256 {
                            return Err(format!(
                                "input {}: reseed identity must be 1 to 256 bytes",
                                input.key
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Convert retained host actions to the same ordered override model. Recorded
/// scheduler positions and runtime fault IDs are observations, never timing inputs.
pub fn merge_interventions(
    inputs: &mut Vec<TimedInput>,
    history: &crate::intervention::History,
) -> Result<(), String> {
    let mut merged = inputs.clone();
    for entry in &history.entries {
        let input = TimedInput {
            key: entry.request_id.clone(),
            at_ns: entry.plan.at_ns,
            action: entry.plan.action.clone().into(),
            timeout_ns: entry
                .plan
                .deadline_ns
                .checked_sub(entry.plan.at_ns)
                .ok_or("invalid override deadline")?,
        };
        if let Some(existing) = merged.iter().find(|i| i.key == input.key) {
            if existing != &input {
                return Err(format!("conflicting input identity {}", input.key));
            }
        } else {
            merged.push(input);
        }
    }
    merged.sort_by_key(|input| input.at_ns);
    validate_program(&merged, u64::MAX)?;
    *inputs = merged;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_host_inputs_merge_by_explicit_identity_ignoring_scheduler_observations() {
        let input = TimedInput {
            key: "host-cut".into(),
            at_ns: 10,
            timeout_ns: 100,
            action: Action::Partition {
                handle: "cut".into(),
                left: vec!["a".into()],
                right: vec!["b".into()],
            }
            .into(),
        };
        let history = crate::intervention::History {
            entries: vec![crate::intervention::Intervention {
                request_id: input.key.clone(),
                position: Default::default(),
                plan: Plan {
                    id: 99,
                    at_ns: input.at_ns,
                    deadline_ns: 110,
                    action: input.action.fault().expect("a fault input").clone(),
                },
            }],
            ..Default::default()
        };
        let mut inputs = vec![input.clone()];
        merge_interventions(&mut inputs, &history).unwrap();
        assert_eq!(inputs.as_slice(), std::slice::from_ref(&input));
        let mut legitimate_same_time = history.clone();
        legitimate_same_time.entries[0].request_id = "another-cut".into();
        merge_interventions(&mut inputs, &legitimate_same_time).unwrap();
        assert_eq!(inputs.len(), 2);
        let before = inputs.clone();
        let mut conflict = history;
        conflict.entries[0].plan.deadline_ns += 1;
        assert!(merge_interventions(&mut inputs, &conflict).is_err());
        assert_eq!(inputs, before);
    }
}
