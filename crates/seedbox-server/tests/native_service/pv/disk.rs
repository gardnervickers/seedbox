//! Named-volume disks of Driver PV service machines: repeatability, worker
//! counts and checkpoint continuation through the public service.
use crate::scenarios::CpuBackend;

#[tokio::test]
async fn pv_volume_disk_repeats_on_one_and_four_workers() {
    crate::scenarios::disk_repeat_workers(CpuBackend::Pv).await;
}

#[tokio::test]
async fn pv_volume_disk_checkpoint_continues_to_uninterrupted_witness() {
    crate::scenarios::disk_checkpoint_continuation(CpuBackend::Pv).await;
}
