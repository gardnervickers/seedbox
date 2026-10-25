//! A completed simulation's failure timeline, read from its retained
//! evidence: program inputs, faults and trigger receipts, lifecycle events,
//! choices and property evidence, in one simulated-time order. What the
//! evidence does not keep is listed, never guessed. No server builds
//! explanations yet: requests are refused.
//!
//! `GET /v1/simulations/{id}/explanation` returns an [`Explanation`].
use crate::SimulationId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Explanation {
    pub simulation: SimulationId,
    /// Ordered by simulated time; equal times keep evidence order.
    pub entries: Vec<ExplanationEntry>,
    /// What the retained evidence leaves out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExplanationEntry {
    pub at_ns: u64,
    pub kind: ExplanationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<u32>,
    /// A stable identity: `input:KEY`, `fault:ID`, `trigger:ID`,
    /// `machine:NAME#INCARNATION`, `choice:MACHINE/SITE#OCCURRENCE`,
    /// `event:NAME`, `state:MACHINE/NAME=VALUE`,
    /// `transition:MACHINE/NAME:FROM>TO` or `property:ID`.
    pub id: String,
    pub detail: String,
    /// Identities this entry follows from or caused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// A property's first violation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub violation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExplanationKind {
    /// A program input other than a fault: anchor, reseed, trigger or choice.
    Input,
    Fault,
    Trigger,
    Lifecycle,
    Ready,
    Exit,
    Control,
    Choice,
    Setup,
    Event,
    State,
    Transition,
    Property,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_link_by_stable_identity_and_omissions_are_explicit() {
        let wire = serde_json::json!({
            "simulation": "k7m2p9qx",
            "entries": [
                {"atNs": 4_000_000_000u64, "kind": "fault", "machine": "db", "id": "fault:3",
                 "detail": "crash db", "links": ["input:gen-2"]},
                {"atNs": 4_200_000_000u64, "kind": "property", "machine": "client", "incarnation": 0,
                 "id": "property:acked writes survive", "detail": "failed", "violation": true},
            ],
            "omissions": ["choices after the first 4096"],
        });
        let explanation: Explanation = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(explanation.entries[1].kind, ExplanationKind::Property);
        assert!(!explanation.entries[0].violation);
        assert_eq!(serde_json::to_value(&explanation).unwrap(), wire);
    }
}
