//! Public factory metadata tests: no run, worker, QEMU or guest execution.
use seedbox_machine::{
    application::{GuestEnvironment, NamedVolume},
    definition::DiskImage,
    machine::Machine,
    snapshot::DecodeLimits,
    test_support::{assets, factory_definition},
};
use seedbox_machine_tcg::machine::tcg::{Factory, Snapshot, TcgMachine};
use std::{collections::BTreeMap, fs, path::Path};
fn entries(wire: &[u8]) -> BTreeMap<String, Vec<u8>> {
    // Controlled structural kernel/firmware fixtures contain no newc signature.
    // Read the real initramfs retained in the opaque public snapshot stream.
    let start = wire.windows(6).position(|s| s == b"070701").unwrap();
    let bytes = &wire[start..];
    let mut at = 0;
    let mut out = BTreeMap::new();
    loop {
        assert_eq!(&bytes[at..at + 6], b"070701");
        let n = |field: usize| {
            u32::from_str_radix(
                std::str::from_utf8(&bytes[at + 6 + field * 8..at + 14 + field * 8]).unwrap(),
                16,
            )
            .unwrap() as usize
        };
        let end = at + 110 + n(11);
        let name = std::str::from_utf8(&bytes[at + 110..end - 1])
            .unwrap()
            .to_owned();
        let data = (end + 3) & !3;
        let size = n(6);
        out.insert(name.clone(), bytes[data..data + size].to_vec());
        at = (data + size + 3) & !3;
        if name == "TRAILER!!!" {
            break;
        }
    }
    out
}
fn wire(snapshot: &Snapshot) -> Vec<u8> {
    let mut bytes = vec![];
    snapshot.write_to(&mut bytes).unwrap();
    bytes
}
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 8 << 20,
        allocation_bytes: 32 << 20,
        collection_items: 65536,
        total_items: 262144,
        string_bytes: 1 << 20,
        blob_bytes: 8 << 20,
    }
}
fn env() -> GuestEnvironment {
    let mut e = GuestEnvironment::new("app", "127.0.0.1 localhost\n10.0.0.1 app\n", false);
    e.named_volumes.push(NamedVolume {
        name: "data".into(),
        target: "/data".into(),
    });
    e.configs.insert("/etc/config".into(), vec![0, 255, 0, 10]);
    e
}
const MACHINE_ID: u16 = 62;
const FOREIGN_MACHINE_ID: u16 = 183;
fn setup(root: &Path, formatter: bool) {
    fs::write(root.join("kernel"), assets::bzimage_header()).unwrap();
    fs::write(root.join("init"), assets::image(MACHINE_ID)).unwrap();
    fs::write(root.join("command-line"), "rdinit=/init").unwrap();
    fs::write(root.join("firmware"), [0x42; 65536]).unwrap();
    if formatter {
        fs::write(root.join("mke2fs"), assets::image(MACHINE_ID)).unwrap();
    }
}

#[test]
fn configured_creation_owns_boot_inputs_after_asset_source_destruction_without_host_exec() {
    let assets_dir = tempfile::tempdir().unwrap();
    let root = assets_dir.path();
    setup(root, true);
    let executable = root.join("not-executable");
    fs::write(&executable, b"not executable; construction must not run it").unwrap();
    let mut factory = Factory::new(&executable, root, 123456).unwrap();
    let mut definition = factory_definition::definition(0);
    definition.disk = Some(DiskImage::zeroed(1 << 30).unwrap());
    let original = definition.clone();
    let environment = env();
    let before = environment.clone();
    let mut first = factory
        .create_configured(&definition, &environment)
        .unwrap();
    let snapshot = first.snapshot().unwrap();
    let initial = wire(&snapshot);
    let parsed = entries(&initial);
    assert!(parsed.contains_key("seedbox/environment.bin"));
    assert_eq!(parsed["seedbox/mke2fs"], assets::image(MACHINE_ID));
    assert!(parsed["seedbox/environment.bin"]
        .windows(4)
        .any(|p| p == [0, 255, 0, 10]));
    drop(first);
    fs::remove_dir_all(root).unwrap();
    drop(environment);
    let mut second = factory.create_configured(&definition, &before).unwrap();
    assert_eq!(wire(&second.snapshot().unwrap()), initial);
    assert_eq!(definition, original);
    drop(factory);
    drop(second);
    drop(snapshot);
    for _ in 0..2 {
        let decoded = Snapshot::read_from(&mut &initial[..], &limits()).unwrap();
        assert_eq!(wire(&decoded), initial);
        let mut restored = TcgMachine::from_snapshot(&executable, &decoded).unwrap();
        assert_eq!(wire(&restored.snapshot().unwrap()), initial);
    }
}
#[test]
fn ordinary_creation_needs_no_formatter_and_configured_named_volumes_fail_before_guest_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    setup(root, false);
    let executable = root.join("not-executable");
    fs::write(&executable, b"data").unwrap();
    let mut factory = Factory::new(&executable, root, 123456).unwrap();
    let mut definition = factory_definition::definition(0);
    let mut plain = factory.create(&definition).unwrap();
    let parsed = entries(&wire(&plain.snapshot().unwrap()));
    assert!(!parsed.contains_key("seedbox/environment.bin"));
    assert!(!parsed.contains_key("seedbox/mke2fs"));
    let environment = env();
    assert!(factory
        .create_configured(&definition, &environment)
        .err()
        .unwrap()
        .to_string()
        .contains("machine disk"));
    definition.disk = Some(DiskImage::zeroed(1 << 30).unwrap());
    assert!(factory
        .create_configured(&definition, &environment)
        .err()
        .unwrap()
        .to_string()
        .contains("mke2fs"));
    let simple = GuestEnvironment::new("app", "", true);
    let mut configured = factory.create_configured(&definition, &simple).unwrap();
    let p = entries(&wire(&configured.snapshot().unwrap()));
    assert!(p.contains_key("seedbox/environment.bin"));
    assert!(!p.contains_key("seedbox/mke2fs"));
    fs::write(root.join("mke2fs"), assets::image(FOREIGN_MACHINE_ID)).unwrap();
    assert!(Factory::new(&executable, root, 123456).is_err());
    let mut dynamic = assets::image(MACHINE_ID);
    dynamic.resize(176, 0);
    dynamic[56..58].copy_from_slice(&2u16.to_le_bytes());
    dynamic[120..124].copy_from_slice(&3u32.to_le_bytes());
    seedbox_machine::machine::assets::elf(&dynamic, MACHINE_ID, false).unwrap();
    fs::write(root.join("mke2fs"), dynamic).unwrap();
    let error = Factory::new(&executable, root, 123456).err().unwrap();
    assert!(error.to_string().contains("static ELF"));
}

fn private_files(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(root: &Path, at: &Path, out: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            assert!(
                !kind.is_symlink(),
                "private files must not escape caller authority"
            );
            if kind.is_dir() {
                visit(root, &path, out);
            } else {
                assert!(
                    entry.metadata().unwrap().len() <= 1 << 20,
                    "bounded structural test inputs only"
                );
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}
#[test]
fn both_factory_paths_use_owned_caller_root_and_restore_uses_new_session_authority() {
    let assets_dir = tempfile::tempdir().unwrap();
    setup(assets_dir.path(), true);
    let executable = assets_dir.path().join("not-executable");
    fs::write(&executable, b"not executable").unwrap();
    let prefix = format!("session-α,offset=7'\\{}", "r".repeat(160));
    let original_root = tempfile::Builder::new().prefix(&prefix).tempdir().unwrap();
    let next_root = tempfile::tempdir().unwrap();
    let unrelated = original_root.path().join("caller-marker");
    fs::write(&unrelated, b"caller-owned").unwrap();
    let path = original_root.path().to_owned();
    let mut factory = Factory::new(&executable, assets_dir.path(), 123456)
        .unwrap()
        .with_scratch_root(&path);
    drop(path);
    let mut definition = factory_definition::definition(0);
    let base = vec![0xa5; 4096];
    definition.disk = Some(DiskImage::from_bytes(base.clone()).unwrap());
    let environment = env();
    let mut records = vec![];
    for configured in [false, true] {
        let mut machine = if configured {
            factory.create_configured(&definition, &environment)
        } else {
            factory.create(&definition)
        }
        .unwrap();
        let files = private_files(original_root.path());
        assert!(files.len() > 1);
        assert!(
            files.values().any(|bytes| bytes == &base),
            "disk base must be in requested root"
        );

        let bytes = wire(&machine.snapshot().unwrap());
        assert!(
            !bytes
                .windows(original_root.path().as_os_str().len())
                .any(|part| part == original_root.path().to_str().unwrap().as_bytes()),
            "host authority must not be a snapshot field"
        );
        records.push(bytes);
        drop(machine);
        assert_eq!(
            private_files(original_root.path()),
            BTreeMap::from([(
                std::path::PathBuf::from("caller-marker"),
                b"caller-owned".to_vec()
            )])
        );
    }
    drop(factory);
    fs::remove_file(&unrelated).unwrap();
    fs::remove_dir(original_root.path()).unwrap();
    assert!(!original_root.path().exists());
    for bytes in records {
        let snapshot = Snapshot::read_from(&mut &bytes[..], &limits()).unwrap();
        let mut restored =
            TcgMachine::from_snapshot_in(&executable, &snapshot, next_root.path()).unwrap();
        assert_eq!(wire(&restored.snapshot().unwrap()), bytes);
        assert!(private_files(next_root.path())
            .values()
            .any(|bytes| bytes == &base));
        assert!(!original_root.path().exists());
        drop(restored);
        assert!(private_files(next_root.path()).is_empty());
    }
}
#[test]
fn invalid_explicit_scratch_root_fails_both_creation_paths_without_fallback() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), true);
    let executable = dir.path().join("not-executable");
    fs::write(&executable, b"data").unwrap();
    let parent = tempfile::tempdir().unwrap();
    let missing = parent.path().join("missing");
    let file = parent.path().join("file");
    fs::write(&file, b"caller-file").unwrap();
    let mut definition = factory_definition::definition(0);
    definition.disk = Some(DiskImage::zeroed(1 << 30).unwrap());
    let environment = env();
    for root in [&missing, &file] {
        let mut factory = Factory::new(&executable, dir.path(), 123456)
            .unwrap()
            .with_scratch_root(root);
        for configured in [false, true] {
            let result = if configured {
                factory.create_configured(&definition, &environment)
            } else {
                factory.create(&definition)
            };
            let error = result.err().unwrap();
            assert!(error.to_string().contains(&root.display().to_string()));
            assert!(!missing.exists());
            assert_eq!(fs::read(&file).unwrap(), b"caller-file");
        }
    }
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
}

#[test]
fn non_utf8_caller_root_is_actionable_error_before_creation_or_restore_effects() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let assets_dir = tempfile::tempdir().unwrap();
    setup(assets_dir.path(), true);
    let executable = assets_dir.path().join("not-executable");
    fs::write(&executable, b"data").unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent
        .path()
        .join(OsString::from_vec(b"session-\xff".to_vec()));
    let mut definition = factory_definition::definition(0);
    definition.disk = Some(DiskImage::from_bytes(vec![0xa5; 4096]).unwrap());
    let environment = env();
    let mut factory = Factory::new(&executable, assets_dir.path(), 123456)
        .unwrap()
        .with_scratch_root(&root);
    for configured in [false, true] {
        let result = if configured {
            factory.create_configured(&definition, &environment)
        } else {
            factory.create(&definition)
        };
        let error = result.err().unwrap();
        assert!(
            error.to_string().to_lowercase().contains("utf-8"),
            "{error}"
        );
        assert!(!root.exists());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    }
    let mut factory = Factory::new(&executable, assets_dir.path(), 123456).unwrap();
    let mut stopped = factory
        .create_configured(&definition, &environment)
        .unwrap();
    let snapshot = stopped.snapshot().unwrap();
    let error = TcgMachine::from_snapshot_in(&executable, &snapshot, &root)
        .err()
        .unwrap();
    assert!(
        error.to_string().to_lowercase().contains("utf-8"),
        "{error}"
    );
    assert!(!root.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}
