//! Named-volume durability under public crash/reboot and real retained replay.
#![cfg(all(feature = "kvm-tests", target_os = "linux", target_arch = "x86_64"))]
pub mod support {
    pub mod case_cleanup;
    pub mod native_service;
}
use support::native_service;
#[tokio::test]
async fn public_pv_volume_crash_reboot_preserves_flushed_and_discards_volatile_data() {
    native_service::pv_disk_crash_reboot_replay().await;
}
