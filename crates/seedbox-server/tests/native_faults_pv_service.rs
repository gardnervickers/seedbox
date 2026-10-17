//! Fault application and recovery through the real retained Driver service.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]

#[path = "support/native_service.rs"]
pub mod native_service;

#[tokio::test]
async fn public_pv_throttle_heal_replays_once_with_native_witness() {
    native_service::pv_throttle_heal_replay().await;
}
