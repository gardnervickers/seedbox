//! Network fault families through the real retained Driver service on PV.

use crate::scenarios::CpuBackend;

#[tokio::test]
async fn public_pv_partition_heal_recovers_and_replays() {
    crate::scenarios::network_partition_heal_replay(CpuBackend::Pv).await;
}

#[tokio::test]
async fn public_pv_shape_heal_recovers_and_replays() {
    crate::scenarios::network_shape_heal_replay(CpuBackend::Pv).await;
}

#[tokio::test]
async fn public_pv_drop_connection_aborts_reincarnates_and_replays() {
    crate::scenarios::network_drop_connection_replay(CpuBackend::Pv).await;
}
