//! Shared deterministic policies and guest-memory protocols, independent of CPU backends.
//! Platform adapters supply committed events and actual transmitted bytes.
pub mod connections;
pub mod links;
pub mod network_counters;

pub mod network_faults;

pub mod properties;

pub mod checkpoint;
