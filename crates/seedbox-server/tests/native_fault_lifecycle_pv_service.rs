//! Public lifecycle faults and recovery with real PV payloads and native replay.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]
#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::{pv_lifecycle_replay, LifecyclePair};

#[tokio::test]
async fn public_pv_stop_restart_replays_native_payload() {
    pv_lifecycle_replay(LifecyclePair::StopRestart).await;
}
#[tokio::test]
async fn public_pv_process_pause_resume_replays_native_payload() {
    pv_lifecycle_replay(LifecyclePair::ProcessPauseResume).await;
}
#[tokio::test]
async fn public_pv_machine_pause_resume_replays_native_payload() {
    pv_lifecycle_replay(LifecyclePair::MachinePauseResume).await;
}
#[tokio::test]
async fn public_pv_crash_reboot_replays_native_payload() {
    pv_lifecycle_replay(LifecyclePair::CrashReboot).await;
}
