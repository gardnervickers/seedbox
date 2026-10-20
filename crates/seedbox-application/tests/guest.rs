#[test]
fn large_image_boot_floor_accounts_for_resident_archive_and_root_tmpfs() {
    assert_eq!(
        seedbox_application::guest::minimum_memory_mb(176 * 1024 * 1024).unwrap(),
        656
    );
    assert!(seedbox_application::guest::minimum_memory_mb(200 * 1024 * 1024).unwrap() > 512);
    assert!(seedbox_application::guest::minimum_memory_mb(u64::MAX).is_err());
}
