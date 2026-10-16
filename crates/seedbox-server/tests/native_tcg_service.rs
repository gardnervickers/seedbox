//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.
#![cfg(all(feature = "tcg-tests", target_os = "linux", target_arch = "x86_64"))]

// Shared test-support API also exposes PV-only scenario helpers; exporting the
// module avoids dead-code suppressions in this TCG integration-test binary.
#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::{native_session, DriverBackend};
use std::time::Duration;

#[tokio::test]
async fn retained_driver_tcg_guest_completes_and_cancellation_reaps_owned_inputs() {
    // Existing completion+cancellation budget is unchanged; W1 has separate
    // paired-run scenarios below, each bounded by the same native budget.
    tokio::time::timeout(Duration::from_secs(180), async {
        native_session(DriverBackend::Tcg, &[200_000], false, None).await;
        native_session(DriverBackend::Tcg, &[2_000_000_000], true, None).await;
    })
    .await
    .expect("native completion/cancellation exceeded existing 180-second native budget");
}

#[tokio::test]
async fn fresh_native_tcg_same_submission_witnesses_match() {
    tokio::time::timeout(
        Duration::from_secs(180),
        native_session(DriverBackend::Tcg, &[200_000, 200_000], false, Some(true)),
    )
    .await
    .expect("fresh matching witness pair exceeded 180-second native budget");
}

#[tokio::test]
async fn fresh_native_tcg_different_input_witnesses_do_not_match() {
    tokio::time::timeout(
        Duration::from_secs(180),
        native_session(DriverBackend::Tcg, &[200_000, 400_000], false, Some(false)),
    )
    .await
    .expect("fresh differing witness pair exceeded 180-second native budget");
}
