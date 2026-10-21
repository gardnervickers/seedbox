//! Fault application and recovery through the real retained Driver service.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]

pub mod support {
    pub mod case_cleanup;
    pub mod native_service;
}
use support::native_service;

#[tokio::test]
async fn public_pv_throttle_heal_replays_once_with_native_witness() {
    native_service::pv_throttle_heal_replay().await;
}

#[tokio::test]
async fn public_pv_fault_active_cancellation_joins_and_cleans_native_processes() {
    native_service::pv_fault_cancel().await;
}

#[cfg(feature = "worker-failure-tests")]
#[tokio::test]
async fn public_pv_fault_active_worker_failure_joins_real_monitor_and_cleans_native_processes() {
    native_service::pv_fault_worker_failure().await;
}
