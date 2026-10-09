//! Shared deterministic policies and guest-memory protocols, independent of CPU backends.
//! Platform adapters supply committed events and actual transmitted bytes.
pub mod connections;
pub mod control;
#[allow(clippy::manual_is_multiple_of)] // Preserve the qualified sector arithmetic.
pub mod disk;
pub mod faults;
pub mod links;
pub mod network_counters;
#[allow(clippy::should_implement_trait)] // Preserve the existing RNG API during extraction.
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
#[allow(clippy::drain_collect)] // Preserve reference delivery collection during extraction.
pub mod world;
pub mod world_events;

pub mod window;
#[cfg(test)]
pub(crate) mod world_mock;
pub mod world_windowed;
