mod archive_bytes;
use super::*;
use seedbox_machine::test_support::factory_definition as definition;
use std::fs;
#[test]
fn loaded_assets_are_owned_and_workload_is_reusable() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let kernel = seedbox_machine::test_support::assets::bzimage_header();
    fs::write(root.join("kernel"), &kernel).unwrap();
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
    let definition = definition::definition(0);
    let original = definition.clone();
    let first = factory.create(&definition).unwrap();
    fs::remove_dir_all(root).unwrap();
    let second = factory.create(&definition).unwrap();
    assert_eq!(definition, original);
    match (&first.manifest.boot, &second.manifest.boot) {
        (
            super::super::Boot::Linux {
                kernel: image,
                command_line,
                initramfs,
                ..
            },
            super::super::Boot::Linux {
                initramfs: again, ..
            },
        ) => {
            assert_eq!(image.as_ref(), kernel);
            assert_eq!(command_line, " custom=x\n");
            assert_eq!(initramfs, again);
        }
        _ => panic!("expected Linux"),
    }
    // A disk is owned by the machine's manifest, sharing its image.
    let mut with_disk = definition.clone();
    let image = seedbox_machine::definition::DiskImage::from_bytes(vec![7; 1024]).unwrap();
    with_disk.disk = Some(image.clone());
    let attached = factory.create(&with_disk).unwrap();
    assert_eq!(attached.manifest.disk, Some(image));
    assert!(first.manifest.disk.is_none());
    // A network interface adds the NIC's MAC and the guest's static
    // address; the networkless command line above is unchanged.
    let mut networked = definition.clone();
    networked.network = Some(
        seedbox_machine::definition::NetworkInterface::new(
            seedbox_machine::definition::MacAddress::new([2, 0, 0, 0, 0, 1]).unwrap(),
            std::net::Ipv4Addr::new(10, 0, 0, 1),
            24,
        )
        .unwrap(),
    );
    let attached = factory.create(&networked).unwrap();
    assert_eq!(attached.manifest.network, Some([2, 0, 0, 0, 0, 1]));
    assert!(first.manifest.network.is_none());
    match &attached.manifest.boot {
        super::super::Boot::Linux { command_line, .. } => {
            assert_eq!(command_line, " custom=x\n seedbox.ipv4=10.0.0.1/24")
        }
        _ => panic!("expected Linux"),
    }
    let mut unsupported = definition;
    unsupported.resources.memory_bytes = 1;
    assert!(factory.create(&unsupported).is_err());
}

#[test]
fn assets_for_another_architecture_or_firmware_are_rejected_at_load() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let write_valid = || {
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
        fs::write(root.join("command-line"), "x".repeat(2047)).unwrap();
        fs::write(root.join("firmware"), [0; 64 * 1024]).unwrap();
    };
    let load = || {
        Factory::new(
            &std::env::current_exe().unwrap(),
            root,
            crate::machine::TEST_EPOCH_SECONDS,
        )
    };
    write_valid();
    assert!(load().is_ok());
    let mut riscv = vec![0; 64];
    riscv[48..56].copy_from_slice(b"RISCV\0\0\0");
    riscv[56..60].copy_from_slice(b"RSC\x05");
    let invalid: [(&str, Vec<u8>); 5] = [
        ("kernel", riscv),
        ("init", seedbox_machine::test_support::assets::image(243)),
        ("command-line", vec![b'x'; 2048]),
        ("firmware", vec![0; 64 * 1024 + 1]),
        // OpenSBI-style ELF firmware is not a BIOS image.
        (
            "firmware",
            seedbox_machine::test_support::assets::image(EM_X86_64),
        ),
    ];
    for (name, bytes) in invalid {
        write_valid();
        fs::write(root.join(name), bytes).unwrap();
        assert!(load().is_err(), "{name}");
    }
}

#[test]
fn the_network_address_counts_toward_the_kernel_command_line_limit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let kernel = seedbox_machine::test_support::assets::bzimage_header();
    fs::write(root.join("kernel"), &kernel).unwrap();
    fs::write(
        root.join("init"),
        seedbox_machine::test_support::assets::image(EM_X86_64),
    )
    .unwrap();
    // 2023 bytes fit alone; the 25-byte address takes them past 2047.
    fs::write(root.join("command-line"), "x".repeat(2023)).unwrap();
    fs::write(root.join("firmware"), [1; 64 * 1024]).unwrap();
    let mut factory = Factory::new(
        &std::env::current_exe().unwrap(),
        root,
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let mut definition = definition::definition(0);
    assert!(factory.create(&definition).is_ok());
    definition.network = Some(
        seedbox_machine::definition::NetworkInterface::new(
            seedbox_machine::definition::MacAddress::new([2, 0, 0, 0, 0, 1]).unwrap(),
            std::net::Ipv4Addr::new(10, 0, 0, 1),
            24,
        )
        .unwrap(),
    );
    let error = factory.create(&definition).err().unwrap();
    assert!(error.to_string().contains("2047 bytes"), "{error}");
}
