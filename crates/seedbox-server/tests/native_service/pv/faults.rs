//! Fault application and recovery through the real retained Driver service.

#[tokio::test]
async fn public_pv_throttle_heal_replays_once_with_native_witness() {
    crate::scenarios::pv_throttle_heal_replay().await;
}

#[tokio::test]
async fn public_pv_fault_active_cancellation_joins_and_cleans_native_processes() {
    crate::scenarios::pv_fault_cancel().await;
}

#[cfg(feature = "worker-failure-tests")]
#[tokio::test]
async fn public_pv_fault_active_worker_failure_joins_real_monitor_and_cleans_native_processes() {
    crate::scenarios::pv_fault_worker_failure().await;
}
