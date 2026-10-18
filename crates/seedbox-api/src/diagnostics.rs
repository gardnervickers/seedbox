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
