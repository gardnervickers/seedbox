//! Shared deterministic policies and guest-memory protocols, independent of CPU backends.
//! Platform adapters supply committed events and actual transmitted bytes.
pub mod connections;
pub mod control;
pub mod disk;
pub mod dns;
pub mod faults;
pub mod host_control;
pub mod input_program;
pub mod intervention;
pub mod links;
pub mod log_index;
pub mod network_counters;
pub mod playback;
pub use seedbox_engine_protocol::output_layout;
pub mod rng;
pub use seedbox_engine_protocol::trace;
pub mod triggers;

pub mod network_faults;

pub mod virtqueue;

pub mod virtio_mmio;

pub mod virtio_console;

pub mod virtio_block;

pub mod virtio_network;

pub mod checkpoint;
pub use seedbox_engine_protocol::launch;
pub mod report;
pub mod world;
pub use seedbox_engine_protocol::world_config;
pub mod world_events;

pub mod window;
#[cfg(test)]
pub(crate) mod world_mock;
pub mod world_windowed;

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `bytes`, as digests and witnesses record it.
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes))
}
#[cfg(unix)]
pub mod observation;
