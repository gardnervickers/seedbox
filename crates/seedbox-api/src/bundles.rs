//! Disk-spooled portable simulation transfers. Import registers completed
//! evidence under a fresh ID; it never submits execution. These endpoints use
//! binary bodies, not JSON arrays or client-supplied server filesystem paths.
use serde::{Deserialize, Serialize};

pub const BUNDLE_CONTENT_TYPE: &str = "application/vnd.seedbox.simulation+tar";
pub const BUNDLE_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Query for GET /v1/simulations/{id}/bundle and POST /v1/bundles.
/// The limit counts all wire bytes and must be 1..=BUNDLE_MAX_BYTES.
/// GET requires completed, resolved inputs. POST returns 201 + Simulation;
/// IDs/ownership are new, no worker is started. Neither operation is resumable
/// or idempotent. Invalid archives/limits return InvalidRequest (400).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundleTransferQuery {
    #[serde(default = "default_limit")]
    pub max_bytes: u64,
}
const fn default_limit() -> u64 {
    BUNDLE_MAX_BYTES
}
impl Default for BundleTransferQuery {
    fn default() -> Self {
        Self {
            max_bytes: default_limit(),
        }
    }
}

/// Portable resumable execution state and its complete binary closure.
/// `GET /v1/simulations/{id}/cold-bundle` (a [`BundleTransferQuery`]) will
/// return the same format with a fresh start in place of captured state: the
/// simulation's whole program, its ancestors' consumed inputs first, over
/// its root's preparation, so a branched run reproduces without its start
/// checkpoint. It answers `notImplemented` for now.
pub const CHECKPOINT_CONTENT_TYPE: &str = "application/vnd.seedbox.checkpoint+tar";
