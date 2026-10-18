//! A completed simulation's failure timeline, built from retained evidence:
//! program inputs, faults and trigger receipts, lifecycle events, choices and
//! the property ledger, in one simulated-time order. What the evidence does
//! not keep is listed, never guessed.
use crate::SimulationId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Explanation {
    pub simulation: SimulationId,
    /// Ordered by simulated time; equal times keep source order.
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
    /// A stable identity: `input:KEY`, `fault:ID`, `trigger:KEY`,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
