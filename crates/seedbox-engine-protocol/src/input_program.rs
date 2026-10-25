//! Authored external inputs and their runtime-assigned admission identities.
use crate::control::{Action, Plan, TriggerSpec, PROCESS_EFFECT_TIMEOUT_NS};
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
/// What an input does when due: admit a fault, which takes a fault ID, or
/// arm a rule, which takes none. Both share the `action`-tagged JSON form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum InputAction {
    Fault(Action),
    Rule(RuleAction),
}
impl<'de> Deserialize<'de> for InputAction {
    // Select by tag rather than trying each form in turn, so a malformed
    // fault reports its own error instead of "no variant matched".
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let rule = value
            .get("action")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|tag| RuleAction::TAGS.contains(&tag));
        if rule {
            serde_json::from_value(value).map(Self::Rule)
        } else {
            serde_json::from_value(value).map(Self::Fault)
        }
        .map_err(D::Error::custom)
    }
}
impl From<Action> for InputAction {
    fn from(action: Action) -> Self {
        Self::Fault(action)
    }
}
impl InputAction {
    /// Inputs this protocol defines but no engine carries out yet: the
    /// defined-only faults and every rule, since no Driver arms a program
    /// trigger. Admission refuses them with [`crate::not_implemented`].
    pub fn is_defined_only(&self) -> bool {
        match self {
            Self::Fault(action) => action.is_defined_only(),
            Self::Rule(_) => true,
        }
    }
    pub fn fault(&self) -> Option<&Action> {
        match self {
            Self::Fault(action) => Some(action),
            Self::Rule(_) => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fault(action) => action.name(),
            Self::Rule(rule) => rule.name(),
        }
    }
    /// Arguments that need no world, as admission would check them.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Fault(action) => action.validate(),
            Self::Rule(RuleAction::Trigger(spec)) => spec.validate(),
        }
    }
}
/// Rule inputs. A trigger input arms a typed trigger when due; the trigger,
/// not the input, later admits its effect as a fault with its own ID. The
/// consumed input leaves an [`ArmReceipt`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RuleAction {
    Trigger(Box<TriggerSpec>),
}
impl RuleAction {
    const TAGS: [&'static str; 1] = ["trigger"];
    pub fn name(&self) -> &'static str {
        match self {
            Self::Trigger(_) => "trigger",
        }
    }
}
/// Matching progress of one program-armed trigger, checkpointed with the
/// program cursor so a restored run continues the same count and partial
/// marker match. Nothing fills it until program triggers are implemented.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerProgress {
    pub trigger_id: u32,
    pub status: crate::control::TriggerStatus,
    /// Matching occurrences seen so far.
    pub seen: u64,
    /// Bytes of an output marker matched so far, across chunk boundaries.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub marker_prefix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_at_ns: Option<u64>,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
/// A consumed rule input: the trigger ID its trigger input armed. Trigger IDs
/// increase in program order, interleaved with oracle-armed ones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmReceipt {
    pub key: String,
    pub at_ns: u64,
    pub trigger_id: u32,
}
impl ArmReceipt {
    /// The receipt names its input, at its time, and a later trigger ID.
    pub fn validate(&self, input: &TimedInput, previous_trigger: &mut u32) -> Result<(), String> {
        if !matches!(input.action, InputAction::Rule(RuleAction::Trigger(_)))
            || self.key != input.key
            || self.at_ns != input.at_ns
            || self.trigger_id <= *previous_trigger
        {
            return Err("trigger arm receipt differs from its source".into());
        }
        *previous_trigger = self.trigger_id;
        Ok(())
    }
}
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
    let mut triggers = 0;
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
        if let InputAction::Rule(RuleAction::Trigger(spec)) = &input.action {
            spec.validate_at(input.at_ns)
                .map_err(|error| format!("input {}: {error}", input.key))?;
            // The run's budget is shared with oracle-armed triggers; a program
            // alone must at least fit in it.
            triggers += 1;
            if triggers > crate::control::MAX_TRIGGERS {
                return Err(format!(
                    "input program arms more than {} triggers",
                    crate::control::MAX_TRIGGERS
                ));
            }
        }
        previous = input.at_ns;
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
    fn trigger_inputs_parse_by_tag_and_faults_keep_their_own_errors() {
        let json = serde_json::json!({
            "key": "reboot-after-crash",
            "at_ns": 5,
            "action": {
                "action": "trigger",
                "when": {"kind": "lifecycle", "machine": "db", "transition": "crashed"},
                "nth": 2,
                "then": {"action": "reboot", "machine": "db"},
            },
        });
        let input: TimedInput = serde_json::from_value(json.clone()).unwrap();
        let InputAction::Rule(RuleAction::Trigger(spec)) = &input.action else {
            panic!("not a trigger: {input:?}");
        };
        assert_eq!((spec.nth, spec.delay_ns), (2, 0));
        assert_eq!(input.action.name(), "trigger");
        assert!(input.action.fault().is_none());
        assert_eq!(serde_json::to_value(&input).unwrap(), json);
        validate_program(std::slice::from_ref(&input), 10).unwrap();
        let mut unknown = json.clone();
        unknown["action"]["surprise"] = true.into();
        assert!(serde_json::from_value::<TimedInput>(unknown).is_err());
        let mut zero = json.clone();
        zero["action"]["nth"] = 0.into();
        let zero: TimedInput = serde_json::from_value(zero).unwrap();
        assert!(validate_program(&[zero], 10)
            .unwrap_err()
            .contains("nth counts from 1"));
        let budget: Vec<TimedInput> = (0..=crate::control::MAX_TRIGGERS)
            .map(|n| {
                let mut input = input.clone();
                input.key = format!("t{n}");
                input
            })
            .collect();
        validate_program(&budget[1..], 10).unwrap();
        assert!(validate_program(&budget, 10)
            .unwrap_err()
            .contains("arms more than 64 triggers"));
        let fault: TimedInput = serde_json::from_value(serde_json::json!({
            "key": "crash", "at_ns": 1, "action": {"action": "crash", "machine": "db"}
        }))
        .unwrap();
        assert_eq!(fault.action.fault().unwrap().name(), "crash");
        let error = serde_json::from_value::<TimedInput>(serde_json::json!({
            "key": "crash", "at_ns": 1, "action": {"action": "crash", "machine": "db", "x": 1}
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field"), "{error}");
    }
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
                    action: input.action.fault().unwrap().clone(),
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
