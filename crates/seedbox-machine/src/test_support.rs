//! Opt-in reusable Machine specification bodies and caller-owned test dependencies.
//! No backend or Driver dependency is required to compile these helpers.
// Feature-gated rather than cfg(test): backend crates' tests instantiate this
// common contract suite, and cfg(test) items are invisible to other crates.
pub use crate::machine::{ExitReason, Machine, RunContext, StopReason, Timestamp};
mod rng;
pub use crate::clock::{FixedClock, SimulationClock};
pub use rng::SeededRng;
/// Documented nonzero realtime epoch of the native machine and Driver tests:
/// 2000-01-01T00:00:00Z. A test choice, not a library default.
pub const TEST_EPOCH_SECONDS: u32 = 946_684_800;
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
