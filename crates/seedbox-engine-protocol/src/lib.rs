//! Engine requests and retained evidence formats, independent of execution.
pub mod application_log;
pub mod control;
pub mod crash;
pub mod driver_world;
pub mod fault_status;
#[cfg(unix)]
pub mod host_clock;
pub mod host_control;
pub mod input_program;
pub mod intervention;
pub mod network_counters;
pub mod observation;
pub mod out_of_memory;
pub mod policy_config;
pub mod property;

/// The refusal for an action or input this protocol defines but no engine
/// carries out yet.
pub fn not_implemented(name: &str) -> String {
    format!("{name} is not implemented yet")
}
