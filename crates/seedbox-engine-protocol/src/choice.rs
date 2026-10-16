//! Structured choices: guest requests the engine answers over the SDK reply
//! path, and the evidence a run retains about each selection.
use serde::{Deserialize, Serialize};

/// Choice evidence a run retains; later selections are still answered.
pub const MAX_CHOICES: usize = 4096;
/// The management line prefix of a reply to an SDK session.
pub const REPLY_PREFIX: &str = "SBX_SDK1 ";

/// One answered choice. Its identity is the machine, incarnation, producer
/// session, site and occurrence; times are commit and reply times.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceRecord {
    pub machine: String,
    pub incarnation: u32,
    pub session: u32,
    pub site: String,
    pub occurrence: u64,
    pub alternatives: usize,
    pub selected: usize,
    /// The selected alternative's name.
    pub chosen: String,
    pub requested_at_ns: u64,
    pub reply_at_ns: u64,
    /// The choice input that selected it; otherwise it was drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_key: Option<String>,
}

/// Every choice a run answered, in commit order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceEvidence {
    pub records: Vec<ChoiceRecord>,
    /// Choices answered after [`MAX_CHOICES`] records were retained.
    #[serde(default)]
    pub untracked: u64,
}

/// The reply line for a choice: the session it goes to and what it says.
pub fn reply_line(session: u32, site: &str, occurrence: u64, index: usize) -> Vec<u8> {
    let body =
        serde_json::json!({"choice": {"site": site, "occurrence": occurrence, "index": index}});
    format!("{REPLY_PREFIX}{session} {body}\n").into_bytes()
}
