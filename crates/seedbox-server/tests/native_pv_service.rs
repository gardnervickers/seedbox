//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]

#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::{native_session, DriverBackend};

#[tokio::test]
async fn retained_driver_pv_guest_completes_and_cancellation_reaps_owned_inputs() {
    native_service::native_completion_and_cancellation(DriverBackend::Pv).await;
}
#[tokio::test]
async fn fresh_native_pv_same_submission_witnesses_match() {
    native_session(DriverBackend::Pv, &[200_000, 200_000], false, Some(true)).await;
}
#[tokio::test]
async fn fresh_native_pv_different_input_witnesses_do_not_match() {
    native_session(DriverBackend::Pv, &[200_000, 400_000], false, Some(false)).await;
}

#[tokio::test]
async fn native_pv_same_seed_workers_one_and_four_match() {
    native_service::pv_worker_counts().await;
}
#[tokio::test]
async fn native_pv_different_seed_is_reported() {
    native_service::pv_seed_report().await;
}
#[tokio::test]
async fn native_pv_large_checkpoint_export_through_public_client_under_load() {
    native_service::pv_large_export_under_load().await;
}

#[tokio::test]
async fn native_pv_public_checkpoint_export_import_matches_uninterrupted() {
    native_service::pv_checkpoint_continuation().await;
}
#[tokio::test]
async fn native_pv_checkpoint_with_pending_sleep_and_network_continues() {
    native_service::checkpoint_in_flight_continuation(DriverBackend::Pv).await;
}
#[tokio::test]
async fn native_pv_network_frames_are_observation_only() {
    native_service::pv_network_frames_are_observation_only().await;
}

#[tokio::test]
async fn native_pv_forced_panic_awaits_owned_cleanup_before_rethrow() {
    native_service::native_forced_failure(DriverBackend::Pv, false).await;
}
