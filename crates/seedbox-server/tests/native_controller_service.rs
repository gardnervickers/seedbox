//! Real oracle socket calls through the public service and retained PV Driver.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]
#[path = "support/native_service.rs"]
pub mod native_service;

#[tokio::test]
async fn prepared_fixture_controller_calls_repeat_with_same_input() {
    native_service::pv_controller_repeat().await;
}

#[tokio::test]
async fn prepared_fixture_controller_checkpoint_preserves_oracle_and_pending_call() {
    native_service::pv_controller_checkpoint().await;
}
