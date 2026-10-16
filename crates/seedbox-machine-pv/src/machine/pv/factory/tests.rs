mod archive_bytes;
use super::*;
use seedbox_machine::test_support::factory_definition as definition;
use std::fs;
fn assets(root: &Path) {
    let kernel = super::super::test_image::program(&[super::super::test_image::Op::Spin]);
    fs::write(root.join("kernel"), kernel).unwrap();
    fs::write(
        root.join("init"),
        seedbox_machine::test_support::assets::image(guest::ELF_MACHINE),
    )
    .unwrap();
    fs::write(root.join("command-line"), " console=x\n").unwrap();
    let files = [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        guest::LOADER,
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ];
    let mut archive = tar::Builder::new(vec![]);
    for name in files {
        let content = if name.ends_with(".txt") || name.ends_with(".LIB") {
            b"license".to_vec()
        } else {
            seedbox_machine::test_support::assets::image(guest::ELF_MACHINE)
        };
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(content.len() as u64);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("seedbox/progress/{name}"),
                content.as_slice(),
            )
            .unwrap();
    }
    fs::write(root.join("progress.tar"), archive.into_inner().unwrap()).unwrap();
}
#[test]
fn mandatory_runtime_derivation_owns_inputs_and_preserves_original_workload() {
    let directory = tempfile::tempdir().unwrap();
    assets(directory.path());
    let mut factory = Factory::new(
        &std::env::current_exe().unwrap(),
        directory.path(),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let definition = definition::definition(0);
    let original = definition.clone();
    let first = factory.create(&definition).unwrap();
    fs::remove_dir_all(directory.path()).unwrap();
    let second = factory.create(&definition).unwrap();
    assert_eq!(definition, original);
    assert_eq!(first.initrd, second.initrd);
    assert_eq!(first.command_line, " console=x\n");
    for required in [b"seedbox/progress/bin64/drrun".as_slice(), b"SBXAPP2"]
        .into_iter()
        .chain(
            guest::LAUNCHER_OPTIONS
                .iter()
                .chain(guest::TRANSLATOR_OPTIONS)
                .map(|option| option.as_bytes()),
        )
    {
        assert!(first
            .initrd
            .windows(required.len())
            .any(|bytes| bytes == required));
    }
    // A network interface adds the NIC and the guest's static address;
    // the networkless command line above is unchanged.
    let mut networked = definition.clone();
    networked.network = Some(
        crate::definition::NetworkInterface::new(
            crate::definition::MacAddress::new([2, 0, 0, 0, 0, 1]).unwrap(),
            std::net::Ipv4Addr::new(10, 0, 0, 1),
            24,
        )
        .unwrap(),
    );
    let networked = factory.create(&networked).unwrap();
    assert_eq!(
        networked.command_line,
        " console=x\n seedbox.ipv4=10.0.0.1/24"
    );
    assert_eq!(networked.network, Some([2, 0, 0, 0, 0, 1]));
    assert_eq!(networked.initrd, first.initrd);
    assert_eq!(first.network, None);
    let mut unsupported = definition;
    unsupported.resources.memory_bytes = 1;
    assert!(factory.create(&unsupported).is_err());
}
#[test]
fn missing_runtime_is_an_error_without_uninstrumented_fallback() {
    let directory = tempfile::tempdir().unwrap();
    assets(directory.path());
    fs::remove_file(directory.path().join("progress.tar")).unwrap();
    assert!(Factory::new(
        &std::env::current_exe().unwrap(),
        directory.path(),
        crate::machine::TEST_EPOCH_SECONDS
    )
    .is_err());
}
#[test]
fn runtime_preload_and_executable_modes_are_required_at_load() {
    for broken in ["libdrpreload.so", "bin64/drrun", guest::LOADER] {
        let directory = tempfile::tempdir().unwrap();
        assets(directory.path());
        let bytes = fs::read(directory.path().join("progress.tar")).unwrap();
        let mut source = tar::Archive::new(bytes.as_slice());
        let mut changed = tar::Builder::new(vec![]);
        for entry in source.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            if broken == "libdrpreload.so" && path.ends_with(broken) {
                continue;
            }
            let mut header = entry.header().clone();
            if broken != "libdrpreload.so" && path.ends_with(broken) {
                header.set_mode(0o644);
                header.set_cksum();
            }
            changed.append_data(&mut header, &path, &mut entry).unwrap();
        }
        fs::write(
            directory.path().join("progress.tar"),
            changed.into_inner().unwrap(),
        )
        .unwrap();
        let error = match Factory::new(
            &std::env::current_exe().unwrap(),
            directory.path(),
            crate::machine::TEST_EPOCH_SECONDS,
        ) {
            Ok(_) => panic!("broken runtime accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(broken), "{error}");
        assert!(
            error.contains(if broken == "libdrpreload.so" {
                "missing"
            } else {
                "lacks execute permission"
            }),
            "{error}"
        );
    }
}
