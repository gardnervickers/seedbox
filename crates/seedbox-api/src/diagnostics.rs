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
    /// Best-effort observed kernel kills; the world retains at most 64 per machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_memory_kills: Vec<OutOfMemoryDiagnostic>,
    pub files: Vec<DiagnosticFile>,
    pub omissions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutOfMemoryDiagnostic {
    pub machine: String,
    pub report: crate::OutOfMemoryKill,
    #[serde(rename = "memoryMiB", skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
}
impl OutOfMemoryDiagnostic {
    pub fn message(&self) -> String {
        let kill = &self.report.kill;
        let memory = self
            .memory_mib
            .map_or_else(String::new, |mib| format!(" (now {mib})"));
        format!("{}: kernel out-of-memory killer killed {:?} (pid {}, anon-rss {} KiB, boot {}); raise memoryMiB{}",
            self.machine, kill.command, kill.pid, kill.anon_rss_kib, self.report.incarnation, memory)
    }
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
