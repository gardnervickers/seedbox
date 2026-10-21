//! Public lifecycle faults and recovery with real PV payloads and native replay.
use crate::scenarios::{pv_lifecycle_replay, LifecyclePair};

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
