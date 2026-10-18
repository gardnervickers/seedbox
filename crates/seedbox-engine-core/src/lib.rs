//! Shared deterministic policies and guest-memory protocols, independent of CPU backends.
//! Platform adapters supply committed events and actual transmitted bytes.
pub mod connections;
pub mod control;
pub mod faults;
pub mod links;
pub mod log_index;
pub mod network_counters;
pub use seedbox_engine_protocol::output_layout;
pub mod rng;
pub use seedbox_engine_protocol::trace;
pub mod triggers;

pub mod network_faults;

pub mod checkpoint;
pub use seedbox_engine_protocol::world_config;

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `bytes`, as digests and witnesses record it.
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes))
}
