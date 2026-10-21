//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.

use crate::scenarios::{native_session, CpuBackend};

#[tokio::test]
async fn retained_driver_tcg_guest_completes_and_cancellation_reaps_owned_inputs() {
    crate::scenarios::native_completion_and_cancellation(CpuBackend::Tcg).await;
}
#[tokio::test]
async fn fresh_native_tcg_same_submission_witnesses_match() {
    native_session(CpuBackend::Tcg, &[200_000, 200_000], false, Some(true)).await;
}
#[tokio::test]
async fn fresh_native_tcg_different_input_witnesses_do_not_match() {
    native_session(CpuBackend::Tcg, &[200_000, 400_000], false, Some(false)).await;
}

#[tokio::test]
async fn native_tcg_forced_timeout_awaits_owned_cleanup_before_rethrow() {
    crate::scenarios::native_forced_failure(CpuBackend::Tcg, true).await;
}

#[tokio::test]
async fn native_tcg_checkpoint_with_pending_sleep_and_network_continues() {
    crate::scenarios::checkpoint_in_flight_continuation(CpuBackend::Tcg).await;
}
