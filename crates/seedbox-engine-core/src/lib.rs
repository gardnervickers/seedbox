//! Shared deterministic policies, independent of CPU backends and guest devices.
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
