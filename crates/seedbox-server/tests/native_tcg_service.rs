//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.
#![cfg(all(feature = "tcg-tests", target_os = "linux", target_arch = "x86_64"))]

// Shared test-support API also exposes PV-only scenario helpers; exporting the
// module avoids dead-code suppressions in this TCG integration-test binary.
#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::{native_session, DriverBackend, HANG_GUARD};

#[tokio::test]
async fn retained_driver_tcg_guest_completes_and_cancellation_reaps_owned_inputs() {
    // Each execution fails at its simulated bound (support/native_service.rs);
    // HANG_GUARD only catches one that stops making simulated progress.
    tokio::time::timeout(HANG_GUARD, async {
        native_session(DriverBackend::Tcg, &[200_000], false, None).await;
        native_session(DriverBackend::Tcg, &[2_000_000_000], true, None).await;
    })
    .await
    .expect("native completion/cancellation hit the host hang guard without finishing");
}

#[tokio::test]
async fn fresh_native_tcg_same_submission_witnesses_match() {
    tokio::time::timeout(
        HANG_GUARD,
        native_session(DriverBackend::Tcg, &[200_000, 200_000], false, Some(true)),
    )
    .await
    .expect("fresh matching witness pair hit the host hang guard without finishing");
}

#[tokio::test]
async fn fresh_native_tcg_different_input_witnesses_do_not_match() {
    tokio::time::timeout(
        HANG_GUARD,
        native_session(DriverBackend::Tcg, &[200_000, 400_000], false, Some(false)),
    )
    .await
    .expect("fresh differing witness pair hit the host hang guard without finishing");
}
