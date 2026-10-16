//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.
#![cfg(all(feature = "tcg-tests", target_os = "linux", target_arch = "x86_64"))]

// Shared test-support API also exposes PV-only scenario helpers; exporting the
// module avoids dead-code suppressions in this TCG integration-test binary.
#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::{native_session, DriverBackend};

#[tokio::test]
async fn retained_driver_tcg_guest_completes_and_cancellation_reaps_owned_inputs() {
    native_service::native_completion_and_cancellation(DriverBackend::Tcg).await;
}
#[tokio::test]
async fn fresh_native_tcg_same_submission_witnesses_match() {
    native_session(DriverBackend::Tcg, &[200_000, 200_000], false, Some(true)).await;
}
#[tokio::test]
async fn fresh_native_tcg_different_input_witnesses_do_not_match() {
    native_session(DriverBackend::Tcg, &[200_000, 400_000], false, Some(false)).await;
}

#[tokio::test]
async fn native_tcg_forced_timeout_awaits_owned_cleanup_before_rethrow() {
    native_service::native_forced_failure(DriverBackend::Tcg, true).await;
}

#[tokio::test]
async fn native_tcg_checkpoint_with_pending_sleep_and_network_continues() {
    native_service::checkpoint_in_flight_continuation(DriverBackend::Tcg).await;
}
