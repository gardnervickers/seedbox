//! Portable archive/parser conformance. Synthetic ELF and bzImage bytes are never executed.
use flate2::read::GzDecoder;
use seedbox::runtime_assets::{AssetSelection, RuntimeAssets};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, io::Read, os::unix::fs::PermissionsExt};

#[path = "support/linux_assets.rs"]
mod fixtures;
use fixtures::{archive, archive_entries, files, gzip, hash, manifest};
const PLATFORM: &str = "x86_64-linux";
fn reindexed_archive(mut files: BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut provenance: Value = serde_json::from_slice(&files["provenance.json"]).unwrap();
    for name in ["bzImage", "compose-init", "busybox", "mke2fs"] {
        provenance["artifacts"][name] = json!(hash(&files[name]));
    }
    files.insert(
        "provenance.json".into(),
        serde_json::to_vec(&provenance).unwrap(),
    );
    archive(&files, &manifest(&files))
}
fn assert_rejected(bytes: &[u8], selection: AssetSelection) {
    let parent = tempfile::tempdir().unwrap();
    fs::write(parent.path().join("sentinel"), b"unrelated").unwrap();
    assert!(RuntimeAssets::install(bytes, parent.path(), PLATFORM, selection).is_err());
    assert_eq!(
        fs::read(parent.path().join("sentinel")).unwrap(),
        b"unrelated"
    );
    let names = fs::read_dir(parent.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(names, ["sentinel"], "failed install leaked scratch state");
}

#[test]
fn selected_kernel_is_owned_helper_free_and_concurrent_installs_are_independent() {
    let files = files();
    let bytes = archive(&files, &manifest(&files));
    let parent = tempfile::tempdir().unwrap();
    fs::write(parent.path().join("sentinel"), b"unrelated").unwrap();
    let first =
        RuntimeAssets::install(&bytes, parent.path(), PLATFORM, AssetSelection::Kernel).unwrap();
    let first_root = first.input("bzImage").parent().unwrap().to_owned();
    assert_eq!(fs::read(first.input("bzImage")).unwrap(), files["bzImage"]);
    for helper in ["compose-init", "busybox", "mke2fs"] {
        assert!(!first.input(helper).exists());
    }
    let second =
        RuntimeAssets::install(&bytes, parent.path(), PLATFORM, AssetSelection::All).unwrap();
    let second_root = second.input("bzImage").parent().unwrap().to_owned();
    assert_ne!(first_root, second_root);
    assert_eq!(
        fs::metadata(&first_root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["bzImage", "compose-init", "busybox", "mke2fs"] {
        assert_eq!(fs::read(second.input(name)).unwrap(), files[name]);
        assert_eq!(
            fs::metadata(second.input(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            if name == "bzImage" { 0o600 } else { 0o755 }
        );
    }
    fs::write(second.input("busybox"), b"tampered private scratch").unwrap();
    let third =
        RuntimeAssets::install(&bytes, parent.path(), PLATFORM, AssetSelection::All).unwrap();
    assert_eq!(fs::read(third.input("busybox")).unwrap(), files["busybox"]);
    drop(first);
    assert!(!first_root.exists());
    assert!(second_root.exists());
    drop(second);
    assert!(!second_root.exists());
    drop(third);
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
    assert_eq!(
        fs::read(parent.path().join("sentinel")).unwrap(),
        b"unrelated"
    );
}
#[test]
fn manifest_identity_roles_sizes_hashes_and_exact_inventory_are_required() {
    let files = files();
    let original = manifest(&files);
    for (label, field, value) in [
        ("version", "format", json!(2)),
        ("host", "host_platform", json!("aarch64-linux")),
        ("guest", "guest_arch", json!("aarch64")),
        ("unknown", "unexpected", json!(true)),
    ] {
        let mut bad = original.clone();
        bad[field] = value;
        assert_rejected(&archive(&files, &bad), AssetSelection::Kernel);
        assert!(!label.is_empty());
    }
    for (field, value) in [
        ("role", json!("kernel")),
        ("execution_arch", json!("aarch64")),
        ("size", json!(1)),
        ("sha256", json!("0".repeat(64))),
        ("extra", json!(1)),
    ] {
        let mut bad = original.clone();
        bad["files"]["busybox"][field] = value;
        assert_rejected(&archive(&files, &bad), AssetSelection::Kernel);
    }
    let mut missing = original.clone();
    missing["files"].as_object_mut().unwrap().remove("busybox");
    assert_rejected(&archive(&files, &missing), AssetSelection::All);
    let mut extra = files.clone();
    extra.insert("unlisted".into(), vec![1]);
    assert_rejected(&archive(&extra, &original), AssetSelection::All);
    let mut corrupt = files.clone();
    corrupt.get_mut("busybox").unwrap()[100] ^= 1;
    assert_rejected(&archive(&corrupt, &original), AssetSelection::Kernel);
}
#[test]
fn archive_duplicate_link_traversal_absolute_truncation_and_trailing_data_fail_cleanly() {
    let files = files();
    let manifest = manifest(&files);
    let base = std::iter::once((
        "assets.json".to_owned(),
        serde_json::to_vec(&manifest).unwrap(),
        b'0',
    ))
    .chain(
        files
            .iter()
            .map(|(name, bytes)| (name.clone(), bytes.clone(), b'0')),
    )
    .collect::<Vec<_>>();
    for (name, kind) in [
        ("bzImage", b'0'),
        ("../sentinel", b'0'),
        ("/tmp/seedbox-escape", b'0'),
        ("busybox", b'2'),
        ("mke2fs", b'1'),
        ("unexpected", b'0'),
    ] {
        let mut entries = base.clone();
        entries.insert(1, (name.into(), vec![1], kind));
        assert_rejected(&archive_entries(&entries), AssetSelection::All);
    }
    let valid = archive(&files, &manifest);
    assert_rejected(&valid[..valid.len() - 4], AssetSelection::All);
    let mut trailing = valid.clone();
    trailing.extend_from_slice(b"trailing");
    assert_rejected(&trailing, AssetSelection::All);
    let mut concatenated = valid.clone();
    concatenated.extend_from_slice(&valid);
    assert_rejected(&concatenated, AssetSelection::All);
    let mut raw = Vec::new();
    GzDecoder::new(valid.as_slice())
        .read_to_end(&mut raw)
        .unwrap();
    assert!(raw.ends_with(&[0; 1024]));
    assert_rejected(&gzip(&raw[..raw.len() - 1024]), AssetSelection::All);
    raw.extend_from_slice(b"nonzero trailing tar content");
    assert_rejected(&gzip(&raw), AssetSelection::All);
    let mut huge = tar::Header::new_ustar();
    huge.set_path("bzImage").unwrap();
    huge.set_mode(0o600);
    huge.set_size(1u64 << 40);
    huge.set_cksum();
    assert_rejected(&gzip(huge.as_bytes()), AssetSelection::All);
}
#[test]
fn binary_parsers_validate_actual_elf_program_bounds_and_x86_boot_headers_even_unselected() {
    for (offset, replacement) in [
        (0, vec![0]),
        (4, vec![1]),
        (5, vec![2]),
        (16, 1u16.to_le_bytes().to_vec()),
        (18, 183u16.to_le_bytes().to_vec()),
        (24, 128u64.to_le_bytes().to_vec()),
        (32, u64::MAX.to_le_bytes().to_vec()),
        (52, 1u16.to_le_bytes().to_vec()),
        (54, 1u16.to_le_bytes().to_vec()),
        (56, 0u16.to_le_bytes().to_vec()),
        (64, 3u32.to_le_bytes().to_vec()),
        (64, 2u32.to_le_bytes().to_vec()),
        (68, 4u32.to_le_bytes().to_vec()),
        (72, u64::MAX.to_le_bytes().to_vec()),
        (80, u64::MAX.to_le_bytes().to_vec()),
        (96, 129u64.to_le_bytes().to_vec()),
        (104, 1u64.to_le_bytes().to_vec()),
    ] {
        let mut files = files();
        let elf = files.get_mut("busybox").unwrap();
        elf[offset..offset + replacement.len()].copy_from_slice(&replacement);
        assert_rejected(&reindexed_archive(files), AssetSelection::Kernel);
    }
    for (offset, replacement) in [
        (510, vec![0]),
        (514, b"BAD!".to_vec()),
        (518, 0x0100u16.to_le_bytes().to_vec()),
        (0x1f1, vec![255]),
        (0x1f1, vec![0]),
        (0x1f4, 0u32.to_le_bytes().to_vec()),
        (0x1f4, u32::MAX.to_le_bytes().to_vec()),
        (0x211, vec![0]),
        (0x236, 0u16.to_le_bytes().to_vec()),
    ] {
        let mut files = files();
        let kernel = files.get_mut("bzImage").unwrap();
        kernel[offset..offset + replacement.len()].copy_from_slice(&replacement);
        assert_rejected(&reindexed_archive(files), AssetSelection::All);
    }
}

#[test]
fn provenance_and_notice_are_validated_after_payload_hashes_match() {
    let original = files();
    let provenance: Value = serde_json::from_slice(&original["provenance.json"]).unwrap();
    for (label, field, value) in [
        ("version", "format", json!(2)),
        ("revision", "source_revision", json!("A".repeat(40))),
        ("notice", "notice_sha256", json!("0".repeat(64))),
        ("lock", "flake_lock_sha256", json!("0".repeat(64))),
        ("unknown", "extra", json!(true)),
    ] {
        let mut files = original.clone();
        let mut bad = provenance.clone();
        bad[field] = value;
        files.insert("provenance.json".into(), serde_json::to_vec(&bad).unwrap());
        assert_rejected(&archive(&files, &manifest(&files)), AssetSelection::Kernel);
        assert!(!label.is_empty());
    }
    for field in ["recipes", "sources", "artifacts"] {
        let mut files = original.clone();
        let mut bad = provenance.clone();
        let key = bad[field]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        bad[field].as_object_mut().unwrap().remove(&key);
        files.insert("provenance.json".into(), serde_json::to_vec(&bad).unwrap());
        assert_rejected(&archive(&files, &manifest(&files)), AssetSelection::Kernel);
    }
    for notice in [vec![0xff], b" \n\t".to_vec()] {
        let mut files = original.clone();
        let mut bad = provenance.clone();
        bad["notice_sha256"] = json!(hash(&notice));
        files.insert("NOTICE.txt".into(), notice);
        files.insert("provenance.json".into(), serde_json::to_vec(&bad).unwrap());
        assert_rejected(&archive(&files, &manifest(&files)), AssetSelection::All);
    }
}
#[test]
fn dynamic_dependencies_fail_but_dependency_free_static_pie_is_admitted() {
    let mut data = fixtures::elf();
    data.resize(224, 0);
    data[56..58].copy_from_slice(&2u16.to_le_bytes());
    data[96..104].copy_from_slice(&224u64.to_le_bytes());
    data[104..112].copy_from_slice(&224u64.to_le_bytes());
    data[120..124].copy_from_slice(&2u32.to_le_bytes()); // PT_DYNAMIC, after existing PT_LOAD.
    data[128..136].copy_from_slice(&192u64.to_le_bytes());
    data[136..144].copy_from_slice(&192u64.to_le_bytes());
    data[152..160].copy_from_slice(&32u64.to_le_bytes());
    data[160..168].copy_from_slice(&32u64.to_le_bytes());
    data[192..200].copy_from_slice(&1u64.to_le_bytes()); // DT_NEEDED
    let mut bad = files();
    bad.insert("busybox".into(), data.clone());
    let parent = tempfile::tempdir().unwrap();
    let error = RuntimeAssets::install(
        &reindexed_archive(bad),
        parent.path(),
        PLATFORM,
        AssetSelection::Kernel,
    )
    .unwrap_err();
    assert!(error.to_string().contains("DT_NEEDED"), "{error}");
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    data[16..18].copy_from_slice(&3u16.to_le_bytes()); // Static PIE, ET_DYN.
    data[192..200].copy_from_slice(&0u64.to_le_bytes()); // DT_NULL, no dependencies.
    let mut valid = files();
    valid.insert("busybox".into(), data);
    let owner = RuntimeAssets::install(
        &reindexed_archive(valid),
        parent.path(),
        PLATFORM,
        AssetSelection::All,
    )
    .unwrap();
    assert!(owner.input("busybox").is_file());
}
#[test]
fn simultaneous_installs_own_distinct_directories_until_each_owner_drops() {
    let bytes = fixtures::package();
    let parent = tempfile::tempdir().unwrap();
    let (first, second) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            RuntimeAssets::install(&bytes, parent.path(), PLATFORM, AssetSelection::All).unwrap()
        });
        let b = scope.spawn(|| {
            RuntimeAssets::install(&bytes, parent.path(), PLATFORM, AssetSelection::Kernel).unwrap()
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    let first_root = first.input("bzImage").parent().unwrap().to_owned();
    let second_root = second.input("bzImage").parent().unwrap().to_owned();
    assert_ne!(first_root, second_root);
    drop(first);
    assert!(!first_root.exists());
    assert!(second.input("bzImage").exists());
    drop(second);
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn actual_python_package_roundtrips_through_rust_kernel_and_all_installation() {
    use std::{path::Path, process::Command};
    let temporary = tempfile::tempdir().unwrap();
    let assets = temporary.path().join("assets");
    let sources = temporary.path().join("sources");
    let parent = temporary.path().join("installed");
    for path in [&assets, &sources, &parent] {
        fs::create_dir(path).unwrap();
    }
    fs::write(parent.join("sentinel"), b"unrelated").unwrap();
    let payload = files();
    for name in ["bzImage", "compose-init", "busybox", "mke2fs"] {
        fs::write(assets.join(name), &payload[name]).unwrap();
    }
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let prepare = Command::new("python3")
        .args([
            "-c",
            r#"
import pathlib,sys
sys.path.insert(0,sys.argv[1])
from harness.tests.test_linux_runtime import source_archive
for name in ('linux','busybox','e2fsprogs','musl'):
    (pathlib.Path(sys.argv[2])/(name+'.source')).write_bytes(source_archive(name))
"#,
        ])
        .arg(&repository)
        .arg(&sources)
        .output()
        .unwrap();
    assert!(
        prepare.status.success(),
        "{}",
        String::from_utf8_lossy(&prepare.stderr)
    );
    let output = temporary.path().join("runtime.tar.gz");
    let package = Command::new("python3")
        .arg(repository.join("tools/linux-runtime/package.py"))
        .arg("--assets")
        .arg(&assets)
        .arg("--sources")
        .arg(&sources)
        .arg("--output")
        .arg(&output)
        .arg("--revision")
        .arg("a".repeat(40))
        .args(["--dirty", "0"])
        .output()
        .unwrap();
    assert!(
        package.status.success(),
        "{}",
        String::from_utf8_lossy(&package.stderr)
    );
    let bytes = fs::read(output).unwrap();
    let kernel = RuntimeAssets::install(&bytes, &parent, PLATFORM, AssetSelection::Kernel).unwrap();
    let all = RuntimeAssets::install(&bytes, &parent, PLATFORM, AssetSelection::All).unwrap();
    assert_eq!(all.provenance.source_revision, "a".repeat(40));
    assert_eq!(
        fs::read(kernel.input("bzImage")).unwrap(),
        payload["bzImage"]
    );
    for name in ["compose-init", "busybox", "mke2fs"] {
        assert!(!kernel.input(name).exists());
        assert_eq!(fs::read(all.input(name)).unwrap(), payload[name]);
    }
    let kernel_root = kernel.input("bzImage").parent().unwrap().to_owned();
    let all_root = all.input("bzImage").parent().unwrap().to_owned();
    assert_ne!(kernel_root, all_root);
    drop(kernel);
    assert!(!kernel_root.exists());
    assert!(all_root.exists());
    drop(all);
    assert!(!all_root.exists());
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
    assert_eq!(fs::read(parent.join("sentinel")).unwrap(), b"unrelated");
}
