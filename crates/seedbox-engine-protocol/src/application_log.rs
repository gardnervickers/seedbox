//! Application output carried by the guest supervisor's management transport.
//! Frames are bounded and binary-safe. Console and supervisor diagnostics are
//! intentionally outside this format.
use serde::{Deserialize, Serialize};

pub const MAX_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Chunk {
    pub stream: Stream,
    pub bytes: Vec<u8>,
}
