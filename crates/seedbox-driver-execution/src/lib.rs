//! Retained Driver execution: request adaptation, native workers, transport,
//! runtime ownership and the server control client. Configuration parsing, OCI
//! image preparation and orchestration belong to seedbox-application; this crate
//! accepts explicit prepared inputs and never depends on the application layer.

pub mod admission;
pub mod driver_world;
pub mod execution;
#[cfg(any(target_os = "linux", test))]
mod host;
pub mod host_control;
pub mod process_lifetime;
pub mod runtime_assets;
mod worker_process;
use seedbox_artifacts::limits;
use seedbox_artifacts::Context;
pub use seedbox_artifacts::{config, engine, store, Error, Result};

#[cfg(test)]
fn fixture_simulation(oracle: &str) -> seedbox_spec::Simulation {
    use seedbox_spec::*;
    Simulation {
        name: "fixture".into(),
        source: "/fixture/seedbox.yaml".into(),
        services: Default::default(),
        configs: Default::default(),
        network: Network {
            name: "default".into(),
            latency_ns: 1_000_000,
            jitter_ns: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
            links: vec![],
        },
        simulator: Default::default(),
        schedule: Default::default(),
        control: None,
        limits: Limits {
            simulated_time_ns: 60_000_000_000,
            wall_time_ns: 600_000_000_000,
        },
        check: Check {
            service: oracle.into(),
            exit_code: 0,
        },
        seed: None,
        wall_clock_epoch: None,
        machine: Default::default(),
        disk: Default::default(),
    }
}
