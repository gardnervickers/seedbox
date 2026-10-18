#![cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "macos", target_arch = "aarch64")
))]
//! Public factory metadata tests: no run, worker, QEMU or guest execution.
use seedbox_machine::{
    application::{GuestEnvironment, NamedVolume},
    definition::DiskImage,
    machine::Machine,
    snapshot::DecodeLimits,
    test_support::{assets, factory_definition},
};
use seedbox_machine_pv::machine::pv::{Factory, PVMachine, Snapshot};
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
#[cfg(target_arch = "x86_64")]
const MACHINE_ID: u16 = 62;
#[cfg(target_arch = "aarch64")]
const MACHINE_ID: u16 = 183;
#[cfg(target_arch = "x86_64")]
const FOREIGN_MACHINE_ID: u16 = 183;
#[cfg(target_arch = "aarch64")]
const FOREIGN_MACHINE_ID: u16 = 62;
fn setup(root: &Path, formatter: bool) {
    #[cfg(target_arch = "x86_64")]
    let kernel = {
        let mut k = assets::bzimage_header();
        k[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
        k
    };
    #[cfg(target_arch = "aarch64")]
    let kernel = {
        let mut k = vec![0; 4096];
        k[16..24].copy_from_slice(&4096u64.to_le_bytes());
        k[56..60].copy_from_slice(b"ARM\x64");
        k
    };
    fs::write(root.join("kernel"), kernel).unwrap();
    fs::write(root.join("init"), assets::image(MACHINE_ID)).unwrap();
    fs::write(root.join("command-line"), "rdinit=/init").unwrap();
    if formatter {
        fs::write(root.join("mke2fs"), assets::image(MACHINE_ID)).unwrap();
    }
    #[cfg(target_arch = "x86_64")]
    let loader = "lib/ld-linux-x86-64.so.2";
    #[cfg(target_arch = "aarch64")]
    let loader = "lib/ld-linux-aarch64.so.1";
    let mut archive = tar::Builder::new(vec![]);
    for name in [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        loader,
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ] {
        let data = if name.ends_with(".txt") || name.ends_with(".LIB") {
            b"license".to_vec()
        } else {
            assets::image(MACHINE_ID)
        };
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o755);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(data.len() as u64);
        archive
            .append_data(&mut h, format!("seedbox/progress/{name}"), &data[..])
            .unwrap();
    }
    fs::write(root.join("progress.tar"), archive.into_inner().unwrap()).unwrap();
}

#[test]
fn every_cold_factory_funnel_retains_exact_named_policy_without_spawning() {
    use seedbox_machine::definition::BackendPolicy;
    for funnel in 0..3 {
        let root = tempfile::tempdir().unwrap();
        setup(root.path(), false);
        let definition = factory_definition::definition(0);
        let policy = BackendPolicy {
            seed: 73,
            timer_jitter_ns: 31,
            stop_tolerance_ns: 9,
            torn_writes: true,
            full_state: true,
            seeded_pv_speed: true,
            boot_end_evidence: true,
            controller: Some(definition.name.clone()),
        };
        let mut factory = Factory::new(&std::env::current_exe().unwrap(), root.path(), 86400)
            .unwrap()
            .with_policy(policy.clone())
            .unwrap();
        let environment = GuestEnvironment::new("node", "127.0.0.1 localhost\n", true);
        let mut machine = match funnel {
            0 => factory.create(&definition).unwrap(),
            1 => factory
                .create_configured(&definition, &environment)
                .unwrap(),
            _ => {
                let bytes = factory
                    .boot_archive_configured(&definition, &environment)
                    .unwrap();
                factory.create_boot_archive(&definition, bytes).unwrap()
            }
        };
        let snapshot = machine.snapshot().unwrap();
        snapshot
            .validate_policy(&definition.name, Some(&policy))
            .unwrap();
        assert!(snapshot.validate_policy(&definition.name, None).is_err());
        assert!(snapshot
            .validate_policy("wrong-name", Some(&policy))
            .is_err());
        let decoded = Snapshot::read_from(&mut &wire(&snapshot)[..], &limits()).unwrap();
        decoded
            .validate_policy(&definition.name, Some(&policy))
            .unwrap();
        assert!(machine.execution_evidence().unwrap().is_none());
        assert!(machine.state_evidence().unwrap().is_none());
    }
}
#[test]
fn cold_factory_none_and_explicit_default_are_distinct_and_invalid_policy_refuses() {
    use seedbox_machine::definition::BackendPolicy;
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), false);
    let definition = factory_definition::definition(0);
    let mut factory = Factory::new(&std::env::current_exe().unwrap(), root.path(), 86400).unwrap();
    let snapshot = factory.create(&definition).unwrap().snapshot().unwrap();
    snapshot.validate_policy(&definition.name, None).unwrap();
    assert!(snapshot
        .validate_policy(&definition.name, Some(&BackendPolicy::default()))
        .is_err());
    let mut factory = factory.with_policy(BackendPolicy::default()).unwrap();
    let snapshot = factory.create(&definition).unwrap().snapshot().unwrap();
    snapshot
        .validate_policy(&definition.name, Some(&BackendPolicy::default()))
        .unwrap();
    assert!(snapshot.validate_policy(&definition.name, None).is_err());
    let bad = BackendPolicy {
        controller: Some(String::new()),
        ..Default::default()
    };
    assert!(factory.with_policy(bad).is_err());
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
        let mut restored = PVMachine::from_snapshot(&executable, &decoded).unwrap();
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

fn read_u32(bytes: &[u8], at: &mut usize) -> usize {
    let n = u32::from_le_bytes(bytes[*at..*at + 4].try_into().unwrap()) as usize;
    *at += 4;
    n
}
fn list(bytes: &[u8], at: &mut usize) -> Vec<String> {
    let n = read_u32(bytes, at);
    (0..n)
        .map(|_| {
            let n = read_u32(bytes, at);
            let s = std::str::from_utf8(&bytes[*at..*at + n])
                .unwrap()
                .to_owned();
            *at += n;
            s
        })
        .collect()
}
#[test]
fn explicit_progress_budget_is_in_both_launch_paths_and_retained_after_source_teardown() {
    for budget in [1, 37, u32::MAX] {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), false);
        let executable = dir.path().join("not-executable");
        fs::write(&executable, b"data").unwrap();
        let mut factory = Factory::new(&executable, dir.path(), 123456)
            .unwrap()
            .with_progress_budget(std::num::NonZeroU32::new(budget).unwrap());
        let definition = factory_definition::definition(0);
        let mut records = vec![];
        for configured in [false, true] {
            let environment = GuestEnvironment::new("app", "", false);
            let mut machine = if configured {
                factory.create_configured(&definition, &environment)
            } else {
                factory.create(&definition)
            }
            .unwrap();
            let snapshot = machine.snapshot().unwrap();
            let bytes = wire(&snapshot);
            let e = entries(&bytes);
            let recipe = &e["seedbox/application.bin"];
            assert_eq!(&recipe[..8], b"SBXAPP2\0");
            let mut at = 8;
            list(recipe, &mut at);
            let prefix = list(recipe, &mut at);
            let i = prefix
                .iter()
                .position(|s| s == "--blocks-per-progress-call")
                .unwrap();
            assert_eq!(prefix[i + 1], budget.to_string());
            assert_eq!(e.contains_key("seedbox/environment.bin"), configured);
            records.push(bytes);
        }
        drop(factory);
        fs::remove_dir_all(dir.path()).unwrap();
        for bytes in records {
            let retained = Snapshot::read_from(&mut &bytes[..], &limits()).unwrap();
            assert_eq!(wire(&retained), bytes);
            let parsed = entries(&bytes);
            let mut at = 8;
            list(&parsed["seedbox/application.bin"], &mut at);
            let prefix = list(&parsed["seedbox/application.bin"], &mut at);
            let i = prefix
                .iter()
                .position(|s| s == "--blocks-per-progress-call")
                .unwrap();
            assert_eq!(prefix[i + 1], budget.to_string());
        }
    }
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
        assert!(files
            .keys()
            .any(|path| path.file_name().unwrap() == "kernel"));
        assert!(files
            .keys()
            .any(|path| path.file_name().unwrap() == "initrd"));
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
            PVMachine::from_snapshot_in(&executable, &snapshot, next_root.path()).unwrap();
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
    let error = PVMachine::from_snapshot_in(&executable, &snapshot, &root)
        .err()
        .unwrap();
    assert!(
        error.to_string().to_lowercase().contains("utf-8"),
        "{error}"
    );
    assert!(!root.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}
