//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]

#[path = "support/native_service.rs"]
mod native_service;
use native_service::{native_session, DriverBackend, HANG_GUARD};

#[tokio::test]
async fn retained_driver_pv_guest_completes_and_cancellation_reaps_owned_inputs() {
    // Each execution fails at its simulated bound (support/native_service.rs);
    // HANG_GUARD only catches one that stops making simulated progress.
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(HANG_GUARD, async {
        native_session(DriverBackend::Pv, &[200_000], false, None).await;
        native_session(DriverBackend::Pv, &[2_000_000_000], true, None).await;
    })
    .await
    .expect("native completion/cancellation hit the host hang guard without finishing");
}

#[tokio::test]
async fn fresh_native_pv_same_submission_witnesses_match() {
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(
        HANG_GUARD,
        native_session(DriverBackend::Pv, &[200_000, 200_000], false, Some(true)),
    )
    .await
    .expect("fresh matching witness pair hit the host hang guard without finishing");
}

#[tokio::test]
async fn fresh_native_pv_different_input_witnesses_do_not_match() {
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(
        HANG_GUARD,
        native_session(DriverBackend::Pv, &[200_000, 400_000], false, Some(false)),
    )
    .await
    .expect("fresh differing witness pair hit the host hang guard without finishing");
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
