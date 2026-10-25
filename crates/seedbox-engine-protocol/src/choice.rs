//! Structured choices: guest [`Record::Choice`](crate::property::Record::Choice)
//! requests the engine answers over the SDK reply path, and the evidence a
//! run keeps of each answer.
//!
//! A choice's identity is its machine, incarnation, session, site and
//! occurrence. The engine draws the answer from the run's seed unless a due
//! [`ChoiceOverride`](crate::input_program::ChoiceOverride) input selects
//! it, so a run's choices replay exactly and a branch can vary them. No
//! engine answers choices yet: the supervisor does not serve the SDK socket,
//! so `seedbox_sdk::choose` takes the first alternative. Under an engine
//! that does, an SDK that waits [`CHOICE_TIMEOUT_NS`] of guest time without
//! an answer also takes the first alternative, reopens its session so a
//! late answer cannot be misread, and records
//! [`CHOICE_UNANSWERED_EVENT`](crate::property::CHOICE_UNANSWERED_EVENT).
use serde::{Deserialize, Serialize};

/// Answered choices a run keeps; later ones are still answered, and counted.
pub const MAX_CHOICES: usize = 4096;
/// How long, in guest time, the SDK waits for a choice's answer.
pub const CHOICE_TIMEOUT_NS: u64 = 10_000_000_000;

/// One answered choice. Times are simulation time: when the request was
/// committed and when the reply was sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceRecord {
    pub machine: String,
    pub incarnation: u32,
    pub session: u32,
    pub site: String,
    /// This site's requests in the session so far, from 1.
    pub occurrence: u64,
    pub alternatives: u32,
    pub selected: u32,
    /// The selected alternative's name.
    pub chosen: String,
    pub requested_at_ns: u64,
    pub replied_at_ns: u64,
    /// The choice input that selected it; absent when it was drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_key: Option<String>,
}

/// Every choice a run answered, in commit order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceEvidence {
    /// The first [`MAX_CHOICES`] answers.
    pub records: Vec<ChoiceRecord>,
    /// Answers after those.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub untracked: u64,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
