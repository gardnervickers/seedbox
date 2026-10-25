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
/// What an input does when due: admit a fault, which takes a fault ID; arm a
/// rule, which takes none; or mark a branch point, which takes none either.
/// All share the `action`-tagged JSON form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum InputAction {
    Fault(Action),
    Rule(RuleAction),
    Branch(BranchAction),
}
impl<'de> Deserialize<'de> for InputAction {
    // Select by tag rather than trying each form in turn, so a malformed
    // fault reports its own error instead of "no variant matched".
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let tag = value.get("action").and_then(serde_json::Value::as_str);
        if tag.is_some_and(|tag| RuleAction::TAGS.contains(&tag)) {
            serde_json::from_value(value).map(Self::Rule)
        } else if tag.is_some_and(|tag| BranchAction::TAGS.contains(&tag)) {
            serde_json::from_value(value).map(Self::Branch)
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
    /// defined-only faults, every rule (no Driver arms a program trigger or
    /// answers a choice) and every branch point. Admission refuses them with
    /// [`crate::not_implemented`].
    pub fn is_defined_only(&self) -> bool {
        match self {
            Self::Fault(action) => action.is_defined_only(),
            Self::Rule(_) | Self::Branch(_) => true,
        }
    }
    pub fn fault(&self) -> Option<&Action> {
        match self {
            Self::Fault(action) => Some(action),
            Self::Rule(_) | Self::Branch(_) => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fault(action) => action.name(),
            Self::Rule(rule) => rule.name(),
            Self::Branch(branch) => branch.name(),
        }
    }
    /// Arguments that need no world, as admission would check them.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Fault(action) => action.validate(),
            Self::Rule(RuleAction::Trigger(spec)) => spec.validate(),
            Self::Rule(RuleAction::Choice(choice)) => choice.validate(),
            Self::Branch(branch) => branch.validate(),
        }
    }
}
/// Rule inputs. A trigger input arms a typed trigger when due; the trigger,
/// not the input, later admits its effect as a fault with its own ID. A
/// choice input selects the answer to one guest choice. Each consumed input
/// leaves an [`ArmReceipt`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RuleAction {
    Trigger(Box<TriggerSpec>),
    Choice(ChoiceOverride),
}
impl RuleAction {
    const TAGS: [&'static str; 2] = ["trigger", "choice"];
    pub fn name(&self) -> &'static str {
        match self {
            Self::Trigger(_) => "trigger",
            Self::Choice(_) => "choice",
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
/// Select one choice occurrence's alternative instead of drawing it. Its
/// identity is a [`crate::property::Record::Choice`] request's: the machine,
/// its incarnation, the SDK session, the site, and the site's occurrence in
/// that session, counted from 1, exactly as [`crate::property::Reply::Choice`]
/// and [`crate::choice::ChoiceRecord`] count it, so an observed choice can be
/// copied into an override. Once due, it answers that request when it
/// arrives; one that no request matches stays pending until the run ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOverride {
    pub machine: String,
    pub incarnation: u32,
    pub session: u32,
    pub site: String,
    pub occurrence: u64,
    /// The alternative's index.
    pub select: u32,
}
impl ChoiceOverride {
    pub fn validate(&self) -> Result<(), String> {
        if self.machine.is_empty() || self.site.is_empty() || self.occurrence == 0 {
            return Err("a choice input names a machine, a site and an occurrence from 1".into());
        }
        if self.site.len() > crate::property::MAX_ID_BYTES {
            return Err(format!(
                "a choice site holds at most {} bytes",
                crate::property::MAX_ID_BYTES
            ));
        }
        if self.select as usize >= crate::property::MAX_CHOICE_ALTERNATIVES {
            return Err(format!(
                "a choice input selects an alternative index below {}",
                crate::property::MAX_CHOICE_ALTERNATIVES
            ));
        }
        Ok(())
    }
}
/// A consumed rule input. A trigger input records the trigger ID it armed;
/// trigger IDs increase in program order, interleaved with oracle-armed
/// ones. A choice input arms no trigger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmReceipt {
    pub key: String,
    pub at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<u32>,
}
impl ArmReceipt {
    /// The receipt names its input, at its time, and for a trigger a later
    /// trigger ID.
    pub fn validate(&self, input: &TimedInput, previous_trigger: &mut u32) -> Result<(), String> {
        let InputAction::Rule(rule) = &input.action else {
            return Err("rule receipt for another input".into());
        };
        if self.key != input.key || self.at_ns != input.at_ns {
            return Err("rule receipt differs from its source".into());
        }
        match (rule, self.trigger_id) {
            (RuleAction::Trigger(_), Some(id)) if id > *previous_trigger => {
                *previous_trigger = id;
                Ok(())
            }
            (RuleAction::Choice(_), None) => Ok(()),
            _ => Err("rule receipt differs from its source".into()),
        }
    }
}
/// Branch inputs mark where a run may be continued with other draws.
///
/// An anchor is the checkpoint phase at its time: it runs before the
/// ordinary inputs due then. When the `captureAnchors` execution option names
/// it, the engine captures checkpoint [`anchor_capture`]`(key)` there, but only
/// where main's own capture rules allow one: every initial application has
/// been ready, no injected fault is prepared and no other capture is pending.
/// Otherwise the anchor passes uncaptured and its [`BranchReceipt`] says why;
/// the capture is never moved to a later barrier, so a capture always holds
/// the state at its anchor's time and nothing can branch from a skipped one.
/// A capture is only a cache of the execution.
///
/// A reseed at the same time, after its anchor, re-keys every seeded engine
/// stream from then on to `seed` and `identity`: the Driver's per-machine
/// streams (boot entropy for later reboots), seeded clock-fault values,
/// every per-link network stream (loss, jitter, reorder, duplication, burst
/// loss, latency spikes, resets), and the streams of seeded disk faults,
/// preemption and choices. The result is the same in a cold run of the whole
/// program as in a continuation of the capture. It does not touch guest
/// memory: the guest kernel's random pool and every application PRNG carry
/// over unchanged, and neither backend feeds a running guest entropy (PV
/// passes boot entropy in the device tree, TCG a boot seed to its QEMU
/// process). So after a reseed a guest's own randomness changes only through
/// a reboot or through the different timing that re-keyed host draws cause;
/// a run with no seeded network, fault or choice draws after the reseed may
/// not vary at all.
///
/// Neither takes a fault ID; each consumed one leaves a [`BranchReceipt`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum BranchAction {
    Anchor {},
    Reseed { seed: u64, identity: String },
}
/// Bytes in a reseed identity.
pub const MAX_RESEED_IDENTITY: usize = 256;
/// Bytes in an anchor's key, so its capture's checkpoint ID fits main's
/// 128-byte request IDs.
pub const MAX_ANCHOR_KEY: usize = 128 - ANCHOR_CAPTURE_PREFIX.len();
/// Checkpoint IDs of anchor captures start with this; no input key can,
/// since keys hold no `.`.
pub const ANCHOR_CAPTURE_PREFIX: &str = "anchor.";
/// The checkpoint ID of anchor `key`'s capture.
pub fn anchor_capture(key: &str) -> String {
    format!("{ANCHOR_CAPTURE_PREFIX}{key}")
}
impl BranchAction {
    const TAGS: [&'static str; 2] = ["anchor", "reseed"];
    pub fn name(&self) -> &'static str {
        match self {
            Self::Anchor {} => "anchor",
            Self::Reseed { .. } => "reseed",
        }
    }
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Reseed { identity, .. }
                if identity.is_empty() || identity.len() > MAX_RESEED_IDENTITY =>
            {
                Err(format!(
                    "reseed identity must be 1 to {MAX_RESEED_IDENTITY} bytes"
                ))
            }
            _ => Ok(()),
        }
    }
}
/// What became of an anchor capture that `captureAnchors` requested.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnchorCapture {
    /// Checkpoint [`anchor_capture`]`(key)` holds the state at the anchor.
    Captured { checkpoint: String },
    /// No capture was taken, for one of main's checkpoint reasons:
    /// `notReady`, `faultPending`, `checkpointPending`, or `unsupported` on
    /// an engine that cannot capture.
    Skipped {
        reason: crate::host_control::ControlError,
    },
}
/// A consumed branch input: where the program cursor passed it, and for a
/// requested anchor capture what became of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchReceipt {
    pub key: String,
    pub at_ns: u64,
    pub action: BranchAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<AnchorCapture>,
}
impl BranchReceipt {
    pub fn validate(&self, input: &TimedInput) -> Result<(), String> {
        let capture_matches = match (&self.action, &self.capture) {
            (_, None) => true,
            (BranchAction::Anchor {}, Some(AnchorCapture::Captured { checkpoint })) => {
                *checkpoint == anchor_capture(&self.key)
            }
            (BranchAction::Anchor {}, Some(AnchorCapture::Skipped { .. })) => true,
            (BranchAction::Reseed { .. }, Some(_)) => false,
        };
        if self.key != input.key
            || self.at_ns != input.at_ns
            || input.action != InputAction::Branch(self.action.clone())
            || !capture_matches
        {
            return Err("branch receipt differs from its source".into());
        }
        Ok(())
    }
}
/// Key prefixes that only Seedbox may give program inputs: profile faults
/// ([`GENERATED_PREFIX`]), campaign mutations ([`MUTATION_PREFIX`]) and the
/// other inputs a campaign adds ([`CAMPAIGN_PREFIX`]). An authored program
/// (a project's overrides) may not use them, since engines gate inputs by
/// these keys; programs Seedbox retains keep them.
pub const RESERVED_KEY_PREFIXES: [&str; 3] = [GENERATED_PREFIX, MUTATION_PREFIX, CAMPAIGN_PREFIX];
/// Refuse an authored input whose key Seedbox reserves.
pub fn validate_authored_keys(inputs: &[TimedInput]) -> Result<(), String> {
    for input in inputs {
        if let Some(prefix) = RESERVED_KEY_PREFIXES
            .iter()
            .find(|prefix| input.key.starts_with(*prefix))
        {
            return Err(format!(
                "input {}: keys starting {prefix} are reserved for inputs Seedbox generates",
                input.key
            ));
        }
    }
    Ok(())
}
/// Keys of the inputs a campaign adds to its members' programs: a root's
/// `n`th anchor, by ascending time, is [`campaign_anchor_key`]`(n)`, and member
/// `m`'s reseed and `j`th choice input are `campaign-reseed-<m>` and
/// `campaign-choice-<m>-<j>`.
pub const CAMPAIGN_PREFIX: &str = "campaign-";
/// The key of a campaign root's `index`th anchor.
pub fn campaign_anchor_key(index: usize) -> String {
    format!("{CAMPAIGN_PREFIX}anchor-{index}")
}
/// Keys of campaign mutations: trigger inputs a planner adds to a branch,
/// placed after something its parent was seen doing. `mutate-<m>` arms
/// member `m`'s [`Mutation`] and `mutate-<m>-end` its teardown. Their
/// effects, though not their teardowns, are skipped during a quiet period,
/// like generated faults.
pub const MUTATION_PREFIX: &str = "mutate-";
/// Trigger IDs one mutation takes from the run's shared
/// [`MAX_TRIGGERS`](crate::control::MAX_TRIGGERS): its own and its teardown's.
pub const MUTATION_TRIGGERS: usize = 2;
/// A fault a branch places after something its parent did: trigger input
/// `mutate-<m>` arms `trigger`, and `mutate-<m>-end`, a trigger on that
/// effect delayed by `duration_ns`, tears it down (heal, resume, reboot or
/// restart). A branch inherits its ancestors' mutations, so a lineage holds
/// at most `MAX_TRIGGERS / MUTATION_TRIGGERS` of them, less any authored or
/// oracle triggers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub trigger: TriggerSpec,
    pub duration_ns: u64,
}
/// Keys of inputs sampled from a [fault profile](crate::fault_profile): fault
/// `n` starts at `gen-<n>` and is torn down at `gen-<n>-end`. The key alone
/// marks an input as generated, so a resubmitted definition keeps its gates.
/// A generated start is skipped, with a rejection naming why, before every
/// initial application has reported ready and while a quiet period is in
/// force; a skipped start's teardown is skipped with it. Teardowns of starts
/// that ran are never skipped, so the system can recover. No engine applies
/// the gates yet, since nothing generates such inputs.
pub const GENERATED_PREFIX: &str = "gen-";
/// The suffix of a generated fault's teardown key.
pub const GENERATED_END_SUFFIX: &str = "-end";
/// Rejection reasons of skipped generated inputs.
pub const SKIPPED_BEFORE_READY: &str = "skipped: generated before the applications were ready";
pub const SKIPPED_QUIET: &str = "skipped: generated during a quiet period";
pub const SKIPPED_WITH_START: &str = "skipped: its generated start was skipped";

/// What a generated key names: the start or teardown of one generated fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generated<'a> {
    Start { fault: &'a str },
    Teardown { fault: &'a str },
}
impl<'a> Generated<'a> {
    /// `None` for an authored key.
    pub fn of(key: &'a str) -> Option<Self> {
        let rest = key.strip_prefix(GENERATED_PREFIX)?;
        let parsed = match rest.strip_suffix(GENERATED_END_SUFFIX) {
            Some(fault) => Self::Teardown { fault },
            None => Self::Start { fault: rest },
        };
        let (Self::Start { fault } | Self::Teardown { fault }) = parsed;
        (!fault.is_empty()).then_some(parsed)
    }
    /// The key of this fault's start.
    pub fn start_key(&self) -> String {
        let (Self::Start { fault } | Self::Teardown { fault }) = self;
        format!("{GENERATED_PREFIX}{fault}")
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
    let mut trigger_keys = BTreeSet::new();
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
        match &input.action {
            InputAction::Rule(RuleAction::Trigger(spec)) => {
                // The run's budget is shared with oracle-armed triggers; a
                // program alone must at least fit in it.
                triggers += 1;
                if triggers > crate::control::MAX_TRIGGERS {
                    return Err(format!(
                        "input program arms more than {} triggers",
                        crate::control::MAX_TRIGGERS
                    ));
                }
                spec.validate_at(input.at_ns)
                    .and_then(|()| match &spec.when {
                        // A teardown follows a trigger input already in the program.
                        crate::control::EventSelector::Fired { trigger }
                            if !trigger_keys.contains(trigger.as_str()) =>
                        {
                            Err(format!(
                                "fired selector names {trigger}, which is no earlier trigger input"
                            ))
                        }
                        _ => Ok(()),
                    })
            }
            InputAction::Rule(RuleAction::Choice(choice)) => choice.validate(),
            InputAction::Branch(BranchAction::Anchor {}) if input.key.len() > MAX_ANCHOR_KEY => {
                Err(format!("anchor keys hold at most {MAX_ANCHOR_KEY} bytes"))
            }
            InputAction::Branch(branch) => branch.validate(),
            InputAction::Fault(_) => Ok(()),
        }
        .map_err(|error| format!("input {}: {error}", input.key))?;
        if let InputAction::Rule(RuleAction::Trigger(_)) = &input.action {
            trigger_keys.insert(input.key.as_str());
        }
        previous = input.at_ns;
    }
    validate_branches(inputs)
}
/// Branch inputs come first among the inputs due at their time, and a
/// reseed follows an anchor at the same time.
fn validate_branches(inputs: &[TimedInput]) -> Result<(), String> {
    let mut ordinary_at = None;
    let mut anchor_at = None;
    for input in inputs {
        match &input.action {
            InputAction::Branch(branch) => {
                if ordinary_at == Some(input.at_ns) {
                    return Err(format!(
                        "input {}: branch inputs precede the other inputs due at their time",
                        input.key
                    ));
                }
                match branch {
                    BranchAction::Anchor {} => anchor_at = Some(input.at_ns),
                    BranchAction::Reseed { .. } if anchor_at != Some(input.at_ns) => {
                        return Err(format!(
                            "input {}: a reseed follows an anchor at the same time",
                            input.key
                        ));
                    }
                    BranchAction::Reseed { .. } => {}
                }
            }
            _ => ordinary_at = Some(input.at_ns),
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
    fn branch_and_choice_inputs_parse_by_tag_and_order_their_time() {
        let parse = |at_ns: u64, key: &str, action: serde_json::Value| -> TimedInput {
            serde_json::from_value(
                serde_json::json!({"key": key, "at_ns": at_ns, "action": action}),
            )
            .unwrap()
        };
        let anchor = parse(10, "a0", serde_json::json!({"action": "anchor"}));
        let reseed = parse(
            10,
            "a0-reseed",
            serde_json::json!({"action": "reseed", "seed": 7, "identity": "campaign/3"}),
        );
        let choice = parse(
            10,
            "pick",
            serde_json::json!({"action": "choice", "machine": "db", "incarnation": 0, "session": 2,
                               "site": "retry", "occurrence": 1, "select": 1}),
        );
        let crash = parse(
            10,
            "crash",
            serde_json::json!({"action": "crash", "machine": "db"}),
        );
        assert_eq!(
            [&anchor, &reseed, &choice, &crash].map(|i| i.action.name()),
            ["anchor", "reseed", "choice", "crash"]
        );
        assert!(matches!(
            &choice.action,
            InputAction::Rule(RuleAction::Choice(ChoiceOverride {
                occurrence: 1,
                incarnation: 0,
                session: 2,
                ..
            }))
        ));
        assert!([&anchor, &reseed, &choice]
            .iter()
            .all(|i| i.action.fault().is_none()));
        let ordered = [
            anchor.clone(),
            reseed.clone(),
            choice.clone(),
            crash.clone(),
        ];
        validate_program(&ordered, 100).unwrap();
        for input in &ordered {
            let wire = serde_json::to_value(input).unwrap();
            assert_eq!(&serde_json::from_value::<TimedInput>(wire).unwrap(), input);
        }
        // Branch points come before every other input due at their time, and
        // a reseed only after its anchor.
        let late = [crash.clone(), anchor.clone()];
        assert!(validate_program(&late, 100)
            .unwrap_err()
            .contains("branch inputs precede"));
        assert!(validate_program(std::slice::from_ref(&reseed), 100)
            .unwrap_err()
            .contains("a reseed follows an anchor"));
        let mut nameless = reseed.clone();
        nameless.action = InputAction::Branch(BranchAction::Reseed {
            seed: 7,
            identity: String::new(),
        });
        assert!(validate_program(&[anchor.clone(), nameless], 100)
            .unwrap_err()
            .contains("reseed identity"));
        let mut unbounded = choice.clone();
        if let InputAction::Rule(RuleAction::Choice(c)) = &mut unbounded.action {
            c.select = crate::property::MAX_CHOICE_ALTERNATIVES as u32;
        }
        assert!(validate_program(&[unbounded], 100)
            .unwrap_err()
            .contains("below 256"));
        // An anchor takes no arguments.
        assert!(serde_json::from_value::<TimedInput>(serde_json::json!({
            "key": "a1", "at_ns": 1, "action": {"action": "anchor", "seed": 1}
        }))
        .is_err());
    }
    #[test]
    fn rule_and_branch_receipts_match_their_inputs() {
        let choice: TimedInput = serde_json::from_value(serde_json::json!({
            "key": "pick", "at_ns": 3,
            "action": {"action": "choice", "machine": "db", "incarnation": 0, "session": 1,
                       "site": "retry", "occurrence": 1, "select": 0},
        }))
        .unwrap();
        let mut previous = 0;
        let receipt = ArmReceipt {
            key: "pick".into(),
            at_ns: 3,
            trigger_id: None,
        };
        receipt.validate(&choice, &mut previous).unwrap();
        let mut armed = receipt.clone();
        armed.trigger_id = Some(1);
        assert!(armed.validate(&choice, &mut previous).is_err());
        assert_eq!(
            serde_json::to_value(&receipt).unwrap(),
            serde_json::json!({"key": "pick", "at_ns": 3})
        );
        let anchor: TimedInput = serde_json::from_value(serde_json::json!({
            "key": "a0", "at_ns": 3, "action": {"action": "anchor"},
        }))
        .unwrap();
        let mut passed = BranchReceipt {
            key: "a0".into(),
            at_ns: 3,
            action: BranchAction::Anchor {},
            capture: None,
        };
        passed.validate(&anchor).unwrap();
        assert!(passed.validate(&choice).is_err());
        // A requested capture records where it went, or why it was skipped.
        passed.capture = Some(AnchorCapture::Captured {
            checkpoint: anchor_capture("a0"),
        });
        passed.validate(&anchor).unwrap();
        assert_eq!(
            serde_json::to_value(&passed).unwrap()["capture"],
            serde_json::json!({"outcome": "captured", "checkpoint": "anchor.a0"})
        );
        passed.capture = Some(AnchorCapture::Captured {
            checkpoint: "somewhere-else".into(),
        });
        assert!(passed.validate(&anchor).is_err());
        passed.capture = Some(AnchorCapture::Skipped {
            reason: crate::host_control::ControlError::FaultPending,
        });
        passed.validate(&anchor).unwrap();
        assert_eq!(
            serde_json::to_value(&passed).unwrap()["capture"],
            serde_json::json!({"outcome": "skipped", "reason": "faultPending"})
        );
    }
    #[test]
    fn authored_programs_may_not_use_reserved_keys() {
        let input = |key: &str| TimedInput {
            key: key.into(),
            at_ns: 1,
            action: Action::Crash {
                machine: "db".into(),
            }
            .into(),
            timeout_ns: default_timeout_ns(),
        };
        validate_authored_keys(&[input("crash-db"), input("general")]).unwrap();
        for key in ["gen-1", "gen-1-end", "mutate-3", "campaign-anchor-0"] {
            let error = validate_authored_keys(&[input(key)]).unwrap_err();
            assert!(
                error.contains("reserved for inputs Seedbox generates"),
                "{error}"
            );
            // A retained program keeps them: the shape check alone allows it.
            validate_program(&[input(key)], 10).unwrap();
        }
        assert_eq!(campaign_anchor_key(2), "campaign-anchor-2");
    }
    #[test]
    fn programs_fit_the_shared_trigger_budget_and_anchor_keys_their_captures() {
        let trigger = |n: usize| -> TimedInput {
            serde_json::from_value(serde_json::json!({
                "key": format!("t{n}"), "at_ns": 1,
                "action": {"action": "trigger", "when": {"kind": "frame"},
                           "then": {"action": "crash", "machine": "db"}},
            }))
            .unwrap()
        };
        let full: Vec<_> = (0..crate::control::MAX_TRIGGERS).map(trigger).collect();
        validate_program(&full, 10).unwrap();
        let mut over = full;
        over.push(trigger(crate::control::MAX_TRIGGERS));
        assert!(validate_program(&over, 10)
            .unwrap_err()
            .contains("arms more than 64 triggers"));
        let anchor = |key: String| TimedInput {
            key,
            at_ns: 1,
            action: InputAction::Branch(BranchAction::Anchor {}),
            timeout_ns: default_timeout_ns(),
        };
        validate_program(&[anchor("a".repeat(MAX_ANCHOR_KEY))], 10).unwrap();
        assert_eq!(anchor_capture(&"a".repeat(MAX_ANCHOR_KEY)).len(), 128);
        assert!(
            validate_program(&[anchor("a".repeat(MAX_ANCHOR_KEY + 1))], 10)
                .unwrap_err()
                .contains("anchor keys hold at most")
        );
    }
    #[test]
    fn fired_teardowns_follow_an_earlier_trigger_input() {
        let trigger = |key: &str, when: serde_json::Value, then: serde_json::Value| -> TimedInput {
            serde_json::from_value(serde_json::json!({
                "key": key, "at_ns": 1,
                "action": {"action": "trigger", "when": when, "then": then},
            }))
            .unwrap()
        };
        let mutation = trigger(
            "mutate-4",
            serde_json::json!({"kind": "event", "name": "committed"}),
            serde_json::json!({"action": "crash", "machine": "db"}),
        );
        let teardown = trigger(
            "mutate-4-end",
            serde_json::json!({"kind": "fired", "trigger": "mutate-4"}),
            serde_json::json!({"action": "reboot", "machine": "db"}),
        );
        validate_program(&[mutation.clone(), teardown.clone()], 10).unwrap();
        let error = validate_program(&[teardown, mutation], 10).unwrap_err();
        assert!(error.contains("no earlier trigger input"), "{error}");
    }
    #[test]
    fn generated_keys_name_a_start_or_its_teardown() {
        assert_eq!(
            Generated::of("gen-7"),
            Some(Generated::Start { fault: "7" })
        );
        let teardown = Generated::of("gen-7-end").unwrap();
        assert_eq!(teardown, Generated::Teardown { fault: "7" });
        assert_eq!(teardown.start_key(), "gen-7");
        for authored in ["crash-db", "gen-", "gen--end", "general-7"] {
            assert_eq!(Generated::of(authored), None, "{authored}");
        }
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
