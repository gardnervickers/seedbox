//! Named-volume disks of Driver TCG service machines: repeatability, worker
//! counts and checkpoint continuation through the public service.
#![cfg(all(feature = "tcg-tests", target_os = "linux", target_arch = "x86_64"))]
pub mod support {
    pub mod case_cleanup;
    pub mod native_service;
}
use native_service::CpuBackend;
use support::native_service;

#[tokio::test]
async fn tcg_volume_disk_repeats_on_one_and_four_workers() {
    native_service::disk_repeat_workers(CpuBackend::Tcg).await;
}

#[tokio::test]
async fn tcg_volume_disk_checkpoint_continues_to_uninterrupted_witness() {
    native_service::disk_checkpoint_continuation(CpuBackend::Tcg).await;
}
