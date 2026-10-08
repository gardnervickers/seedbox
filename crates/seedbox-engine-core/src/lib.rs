//! Shared deterministic policies and guest-memory protocols, independent of CPU backends.
//! Platform adapters supply committed events and actual transmitted bytes.
#[allow(clippy::should_implement_trait)] // Preserve the existing RNG API during extraction.
#[rustfmt::skip]
pub mod rng;
#[allow(clippy::manual_is_multiple_of)] // Preserve the qualified sector arithmetic.
#[rustfmt::skip]
pub mod disk;
#[rustfmt::skip]
pub mod links;
#[rustfmt::skip]
pub mod connections;
#[rustfmt::skip]
pub mod triggers;
#[rustfmt::skip]
pub mod faults;
#[rustfmt::skip]
pub mod control;
#[rustfmt::skip]
pub mod network_counters;

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
