//! Network fault families through the real retained Driver service on TCG.
#![cfg(all(feature = "tcg-tests", target_os = "linux", target_arch = "x86_64"))]

#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::CpuBackend;

#[tokio::test]
async fn public_tcg_partition_heal_recovers_and_replays() {
    native_service::network_partition_heal_replay(CpuBackend::Tcg).await;
}

#[tokio::test]
async fn public_tcg_shape_heal_recovers_and_replays() {
    native_service::network_shape_heal_replay(CpuBackend::Tcg).await;
}
