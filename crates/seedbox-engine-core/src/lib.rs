//! Shared deterministic policies and guest-memory protocols, independent of CPU backends.
//! Platform adapters supply committed events and actual transmitted bytes.
pub mod connections;
pub mod control;
pub mod disk;
pub mod faults;
pub mod links;
pub mod network_counters;
pub mod rng;
pub mod triggers;

pub mod network_faults;

pub mod virtqueue;

pub mod virtio_mmio;

pub mod virtio_console;

pub mod virtio_block;

pub mod virtio_network;

pub mod pv_progress;

pub mod checkpoint;
pub mod world;
pub mod world_events;

pub mod window;
#[cfg(test)]
pub(crate) mod world_mock;
pub mod world_windowed;
