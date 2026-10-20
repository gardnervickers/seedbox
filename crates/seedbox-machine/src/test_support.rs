//! Opt-in reusable Machine specification bodies and caller-owned test dependencies.
//! No backend or Driver dependency is required to compile these helpers.
// TODO: Re-evaluate: backend-local tests instantiate one common contract suite.
// A cfg(test)-only module cannot be consumed by tests in another crate.
pub use crate::machine::{ExitReason, Machine, RunContext, StopReason, Timestamp};
mod rng;
pub use crate::clock::{FixedClock, SimulationClock};
pub use rng::SeededRng;
pub mod assets;
pub mod disk_scenario;
pub mod factory_definition;
pub mod guest_service_snapshot_tests;
pub mod guest_services;
pub mod kernel_terminal_tests;
pub mod network_scenario;
pub mod prepared_application;
pub mod prepared_fixture;
pub mod prepared_image;
pub mod simulation_control;
pub mod test_environment;
