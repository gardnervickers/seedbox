//! Named-volume disks of Driver TCG service machines: repeatability, worker
//! counts and checkpoint continuation through the public service.
use crate::scenarios::CpuBackend;

#[tokio::test]
async fn tcg_volume_disk_repeats() {
    crate::scenarios::disk_repeat(CpuBackend::Tcg).await;
}

#[tokio::test]
async fn tcg_volume_disk_checkpoint_continues_to_uninterrupted_witness() {
    crate::scenarios::disk_checkpoint_continuation(CpuBackend::Tcg).await;
}
