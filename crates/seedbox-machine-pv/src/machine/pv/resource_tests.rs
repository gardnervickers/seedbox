use super::*;

mod scratch_tests;

#[cfg(target_arch = "x86_64")]
fn kernel() -> Vec<u8> {
    let mut kernel = vec![0; 3072];
    kernel[0x1f1] = 4;
    kernel[0x202..0x206].copy_from_slice(b"HdrS");
    kernel[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
    kernel[0x211] = 1;
    kernel[0x236..0x238].copy_from_slice(&0x21u16.to_le_bytes());
    kernel[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
    kernel[0x258..0x260].copy_from_slice(&0x1000000u64.to_le_bytes());
    kernel[0x260..0x264].copy_from_slice(&(16u32 << 20).to_le_bytes());
    kernel
}
#[cfg(target_arch = "aarch64")]
fn kernel() -> Vec<u8> {
    let mut image = vec![0; 4096];
    image[16..24].copy_from_slice(&4096u64.to_le_bytes());
    image[56..60].copy_from_slice(b"ARM\x64");
    image
}

#[test]
fn deferred_capture_keeps_configured_memory_after_source_destruction() {
    for memory in [64u64, 128, 256, 512, 1024, 2048, 3072] {
        let mut machine = PVMachine::construct_with_memory(
            Path::new("/never-launched"),
            kernel(),
            b"initrd".to_vec(),
            "rdinit=/init".into(),
            None,
            None,
            memory << 20,
        )
        .unwrap();
        let source = machine.directory.path().to_owned();
        let snapshot = machine.capture().unwrap();
        let copy = snapshot.clone();
        assert!(std::sync::Arc::ptr_eq(&snapshot.bytes, &copy.bytes));
        assert!(
            snapshot.bytes.len() < 65536,
            "deferred snapshot allocates no guest RAM"
        );
        drop(machine);
        assert!(!source.exists());
        drop(snapshot);
        for _ in 0..2 {
            let restored = PVMachine::from_snapshot(Path::new("/never-launched"), &copy).unwrap();
            assert_eq!(restored.memory_bytes, memory << 20);
            assert!(restored.worker.is_none());
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn initrd_ceiling_is_applied_before_sparse_placement_and_never_truncated() {
    let ram = boot::RamLayout::native(2048 << 20).unwrap();
    assert!(ram.span as u64 > 1 << 31);
    let (_, address) = boot::layout_for(&ram, &kernel(), &[0; 4096]).unwrap();
    assert_eq!(address, 0x7ffff000);
    let high_ram = boot::RamLayout::native(65536 << 20).unwrap();
    assert_eq!(
        boot::layout_for(&high_ram, &kernel(), &[0; 4096])
            .unwrap()
            .1,
        address
    );
    let mut collision = kernel();
    collision[0x258..0x260].copy_from_slice(&0x09fff000u64.to_le_bytes());
    collision[0x260..0x264].copy_from_slice(&8192u32.to_le_bytes());
    assert!(boot::layout_for(&ram, &collision, &[0; 4096]).is_err());
    let mut overflow = kernel();
    overflow[0x258..0x260].copy_from_slice(&(u64::MAX - 4095).to_le_bytes());
    assert!(boot::layout_for(&ram, &overflow, &[0; 4096]).is_err());
}

#[cfg(target_arch = "x86_64")]
#[test]
fn x86_boot_errors_name_the_asset_and_configured_memory() {
    let check = |bytes, image: &[u8], asset: &str| {
        let ram = boot::RamLayout::native(bytes).unwrap();
        let error = boot::layout_for(&ram, image, b"initrd").unwrap_err();
        assert!(error.contains(asset), "{error}");
        assert!(
            error.contains(&format!("configured RAM ({bytes} bytes)")),
            "{error}"
        );
        assert!(
            error.contains("adjust boot inputs or configured memory"),
            "{error}"
        );
    };
    check(16384, &kernel(), "kernel payload");
    check(16 << 20, &kernel(), "kernel decompression reservation");
    let mut image = kernel();
    image[0x22c..0x230].copy_from_slice(&0x100000u32.to_le_bytes());
    check(64 << 20, &image, "initrd");
}

#[test]
fn arm_boot_errors_name_image_or_initrd_and_configured_memory() {
    let mut image = vec![0; 64];
    image[16..24].copy_from_slice(&(3u64 << 30).to_le_bytes());
    image[56..60].copy_from_slice(b"ARM\x64");
    let ram = boot::RamLayout::arm_test(2048 << 20).unwrap();
    let error = boot::arm_layout_for(&ram, &image, b"initrd").unwrap_err();
    assert!(error.contains("Image placement"), "{error}");
    assert!(
        error.contains("configured RAM (2147483648 bytes)"),
        "{error}"
    );
    image[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let ram = boot::RamLayout::arm_test(65536).unwrap();
    let error = boot::arm_layout_for(&ram, &image, b"initrd").unwrap_err();
    assert!(error.contains("initrd placement"), "{error}");
    assert!(error.contains("configured RAM (65536 bytes)"), "{error}");
}

#[test]
fn restore_scratch_is_released_before_owner_and_on_failure() {
    let source = tempfile::tempdir().unwrap();
    let source_file = source.path().join("capture");
    fs::write(&source_file, b"source remains owned").unwrap();
    let target = tempfile::tempdir().unwrap();
    let image = restore_image(target.path(), b"restore bytes").unwrap();
    let path = image.path().to_owned();
    assert_eq!(fs::read(&path).unwrap(), b"restore bytes");
    remove_restore_image(image).unwrap();
    assert!(!path.exists());
    assert!(
        target.path().exists(),
        "target outlives successful scratch cleanup"
    );
    let (result, failed_path): (Result<(), String>, PathBuf) = {
        let image = restore_image(target.path(), b"failed restore").unwrap();
        (
            Err("worker failed before publication".into()),
            image.path().to_owned(),
        )
    };
    assert!(result.is_err());
    assert!(!failed_path.exists());
    assert_eq!(fs::read(source_file).unwrap(), b"source remains owned");
}

#[test]
fn restore_scratch_cleanup_error_has_owned_path_context() {
    let directory = tempfile::tempdir().unwrap();
    let image = restore_image(directory.path(), b"restore bytes").unwrap();
    let path = image.path().to_owned();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let error = remove_restore_image(image).unwrap_err();
    assert!(error.contains("PV restore image cleanup"), "{error}");
    assert!(error.contains(path.to_str().unwrap()), "{error}");
}
