//! Real oracle socket calls through the public service and retained Driver,
//! on PV (`kvm-tests`) and on TCG (`tcg-tests`).
use crate::scenarios::CpuBackend;

#[cfg(feature = "kvm-tests")]
#[tokio::test]
async fn prepared_fixture_controller_calls_repeat_with_same_input() {
    crate::scenarios::controller_repeat(CpuBackend::Pv).await;
}

#[cfg(feature = "kvm-tests")]
#[tokio::test]
async fn prepared_fixture_controller_checkpoint_preserves_oracle_and_pending_call() {
    crate::scenarios::controller_checkpoint(CpuBackend::Pv).await;
}

#[cfg(feature = "tcg-tests")]
#[tokio::test]
async fn tcg_prepared_fixture_controller_calls_repeat_with_same_input() {
    crate::scenarios::controller_repeat(CpuBackend::Tcg).await;
}

#[cfg(feature = "tcg-tests")]
#[tokio::test]
async fn tcg_prepared_fixture_controller_checkpoint_preserves_oracle_and_pending_call() {
    crate::scenarios::controller_checkpoint(CpuBackend::Tcg).await;
}
