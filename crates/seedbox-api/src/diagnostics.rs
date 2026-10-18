//! Completed simulation raw evidence; names are opaque retained keys, not paths.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticFile {
    pub name: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticFiles {
    pub files: Vec<DiagnosticFile>,
    pub omissions: BTreeMap<String, String>,
    /// Processes the guest kernel killed for lack of memory, by machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_memory_kills: Vec<MachineOutOfMemoryKill>,
}

/// One guest kernel out-of-memory kill and the machine's configured memory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineOutOfMemoryKill {
    pub machine: String,
    /// The machine's guest RAM, when the definition names the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
    pub kill: crate::OutOfMemoryKill,
}

/// Single lossless JSON diagnostic document. Clients may encode payloadHex
/// incrementally from a verified spool instead of allocating the whole string.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticDocument {
    pub file: DiagnosticFile,
    pub payload_hex: String,
    pub omissions: BTreeMap<String, String>,
}

/// Exact retained key and bounded diagnostic response size.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticFileQuery {
    pub name: String,
    #[serde(default = "diagnostic_limit")]
    pub max_bytes: u64,
}
fn diagnostic_limit() -> u64 {
    crate::BUNDLE_MAX_BYTES
}
