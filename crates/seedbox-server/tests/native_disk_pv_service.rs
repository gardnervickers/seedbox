//! Named-volume disks of Driver PV service machines: repeatability, worker
//! counts and checkpoint continuation through the public service.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]
#[path = "support/native_service.rs"]
pub mod native_service;
use native_service::DriverBackend;

#[tokio::test]
async fn pv_volume_disk_repeats_on_one_and_four_workers() {
    native_service::disk_repeat_workers(DriverBackend::Pv).await;
}

#[tokio::test]
async fn pv_volume_disk_checkpoint_continues_to_uninterrupted_witness() {
    native_service::disk_checkpoint_continuation(DriverBackend::Pv).await;
}
