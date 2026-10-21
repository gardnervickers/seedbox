//! Public clock faults use real retained guests, timers and replay comparison.

#[tokio::test]
async fn public_tcg_clock_skew_heal_replays_native_timers_and_clocks() {
    crate::scenarios::tcg_clock_skew_heal_replay().await;
}

#[tokio::test]
async fn public_tcg_clock_drift_heal_replays_native_sleep_rate_and_timers() {
    crate::scenarios::tcg_clock_drift_heal_replay().await;
}
