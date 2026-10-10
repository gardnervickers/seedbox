//! Engine requests and retained evidence formats, independent of execution.
pub mod application_log;
pub mod disk;
#[cfg(unix)]
pub mod host_clock;
pub mod host_control;
pub mod input_program;
pub mod intervention;
pub mod launch;
pub mod log_index;
pub mod network_counters;
pub mod observation;
pub mod output_layout;
pub mod report;
pub mod trace;
pub mod world_config;
