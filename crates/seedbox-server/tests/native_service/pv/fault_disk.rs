//! Named-volume durability under public crash/reboot and real retained replay.
#[tokio::test]
async fn public_pv_volume_crash_reboot_preserves_flushed_and_discards_volatile_data() {
    crate::scenarios::pv_disk_crash_reboot_replay().await;
}
