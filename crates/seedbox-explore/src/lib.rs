//! Passive exploration policy: no processes, workers or async runtime.
//! Callers turn its outputs into ordinary simulation submissions.
pub mod campaign;
pub mod counterfactual;
pub mod faults;
mod rng;
pub mod shrink;
