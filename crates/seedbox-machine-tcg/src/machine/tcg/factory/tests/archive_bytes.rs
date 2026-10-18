use super::*;

#[test]
fn pure_configured_assembly_is_exact_creation_input_and_does_not_gate_ram_sizing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(
        root.join("kernel"),
        seedbox_machine::test_support::assets::bzimage_header(),
    )
    .unwrap();
    fs::write(
        root.join("init"),
        seedbox_machine::test_support::assets::image(EM_X86_64),
    )
    .unwrap();
    fs::write(root.join("command-line"), " custom=x\n").unwrap();
    fs::write(root.join("firmware"), [1; 64 * 1024]).unwrap();
    let mut factory = Factory::new(
        &std::env::current_exe().unwrap(),
        root,
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let mut definition = definition::definition(0);
    let original = definition.clone();
    let mut environment = GuestEnvironment::new("node", "127.0.0.1 localhost\n", true);
    environment
        .configs
        .insert("/etc/first.bin".into(), vec![0xa5; 8 << 20]);
    environment
        .configs
        .insert("/etc/second.bin".into(), vec![0x5a; 8 << 20]);
    fs::remove_dir_all(root).unwrap();
    definition.resources.memory_bytes = 1;
    let expected = factory
        .boot_archive_configured(&definition, &environment)
        .unwrap();
    assert!(
        expected.len() > 16 << 20,
        "referenced config data is not a metadata-envelope quota"
    );
    assert!(factory
        .create_configured(&definition, &environment)
        .is_err());
    definition.resources.memory_bytes = original.resources.memory_bytes;
    assert_eq!(
        factory
            .boot_archive_configured(&definition, &environment)
            .unwrap(),
        expected
    );
    let machine = factory
        .create_configured(&definition, &environment)
        .unwrap();
    let super::super::super::Boot::Linux { initramfs, .. } = &machine.manifest.boot else {
        panic!("expected complete Linux boot image")
    };
    assert_eq!(initramfs.as_ref(), expected.as_slice());
    assert_eq!(definition, original);
    assert!(
        !expected
            .windows(b"seedbox/progress/bin64/drrun".len())
            .any(|part| part == b"seedbox/progress/bin64/drrun"),
        "TCG must not inherit a PV instrumentation prefix"
    );
}
