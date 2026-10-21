//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.

use crate::scenarios::{native_session, CpuBackend};

#[tokio::test]
async fn retained_driver_pv_guest_completes_and_cancellation_reaps_owned_inputs() {
    crate::scenarios::native_completion_and_cancellation(CpuBackend::Pv).await;
}
#[tokio::test]
async fn fresh_native_pv_same_submission_witnesses_match() {
    native_session(CpuBackend::Pv, &[200_000, 200_000], false, Some(true)).await;
}
#[tokio::test]
async fn fresh_native_pv_different_input_witnesses_do_not_match() {
    native_session(CpuBackend::Pv, &[200_000, 400_000], false, Some(false)).await;
}

#[tokio::test]
async fn native_pv_same_seed_workers_one_and_four_match() {
    crate::scenarios::pv_worker_counts().await;
}
#[tokio::test]
async fn native_pv_different_seed_is_reported() {
    crate::scenarios::pv_seed_report().await;
}
#[tokio::test]
async fn native_pv_large_checkpoint_export_through_public_client_under_load() {
    crate::scenarios::pv_large_export_under_load().await;
}

#[tokio::test]
async fn native_pv_public_checkpoint_export_import_matches_uninterrupted() {
    crate::scenarios::pv_checkpoint_continuation().await;
}
#[tokio::test]
async fn native_pv_checkpoint_with_pending_sleep_and_network_continues() {
    crate::scenarios::checkpoint_in_flight_continuation(CpuBackend::Pv).await;
}
#[tokio::test]
async fn native_pv_network_frames_are_observation_only() {
    crate::scenarios::pv_network_frames_are_observation_only().await;
}

#[tokio::test]
async fn native_pv_forced_panic_awaits_owned_cleanup_before_rethrow() {
    crate::scenarios::native_forced_failure(CpuBackend::Pv, false).await;
}
