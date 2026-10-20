//! Network fault families through the real retained Driver service on PV.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]

#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::CpuBackend;

#[tokio::test]
async fn public_pv_partition_heal_recovers_and_replays() {
    native_service::network_partition_heal_replay(CpuBackend::Pv).await;
}

#[tokio::test]
async fn public_pv_shape_heal_recovers_and_replays() {
    native_service::network_shape_heal_replay(CpuBackend::Pv).await;
}

#[tokio::test]
async fn public_pv_drop_connection_aborts_reincarnates_and_replays() {
    native_service::network_drop_connection_replay(CpuBackend::Pv).await;
}
