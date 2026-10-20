//! Opt-in run-wide backend semantics. Scheduling remains with the Driver.
//! The policy is the retained wire type; backends add its snapshot encoding.
pub use seedbox_engine_protocol::driver_world::BackendPolicy;

crate::record!(BackendPolicy {
    seed: u64,
    timer_jitter_ns: u64,
    stop_tolerance_ns: u64,
    torn_writes: bool,
    full_state: bool,
    seeded_pv_speed: bool,
    boot_end_evidence: bool,
    controller: Option<String>,
});
