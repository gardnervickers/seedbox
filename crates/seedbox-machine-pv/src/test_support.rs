//! Opt-in process-level fixtures for tests of the private PV worker entry.
// Feature-gated rather than cfg(test): the standalone worker's tests and the
// CLI executable's tests both assert the same entry contract.
pub mod worker_entry;
