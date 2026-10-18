//! Public clock faults use real retained guests, timers and replay comparison.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]

#[path = "support/native_service.rs"]
pub mod native_service;

#[tokio::test]
async fn public_pv_clock_skew_heal_replays_native_timers_and_clocks() {
    native_service::pv_clock_skew_heal_replay().await;
}

#[tokio::test]
async fn public_pv_clock_drift_heal_replays_native_sleep_rate_and_timers() {
    native_service::pv_clock_drift_heal_replay().await;
}
