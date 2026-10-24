//! Network fault families through the real retained Driver service on TCG.

use crate::scenarios::CpuBackend;

#[tokio::test]
async fn public_tcg_partition_heal_recovers_and_replays() {
    crate::scenarios::network_partition_heal_replay(CpuBackend::Tcg).await;
}

#[tokio::test]
async fn public_tcg_shape_heal_recovers_and_replays() {
    crate::scenarios::network_shape_heal_replay(CpuBackend::Tcg).await;
}
