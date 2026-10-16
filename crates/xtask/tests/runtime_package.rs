//! Preserve helper package metadata and read-only Nix input behavior.
#![cfg(unix)]
mod support;

use std::{
    collections::BTreeSet,
    fs::{self, File, FileTimes},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use support::{archive_members, package_members, python, success, Member};

const NAMES: [&str; 4] = ["NOTICE.txt", "compose-init", "mke2fs", "progress.tar"];

fn package(assets: &Path, output: &Path) {
    success(python("package.py").arg(assets).arg(output), 30);
}

fn assert_expected_members(members: &[Member]) {
    assert_eq!(
        members.len(),
        NAMES.len(),
        "nonempty exact helper member count"
    );
    assert_eq!(
        members
            .iter()
            .map(|m| m.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(NAMES)
    );
}

#[test]
fn archive_reader_rejects_empty_and_malformed_packages() {
    // tar permits an empty archive; a helper package does not. Check both no
    // decompressed bytes and a correctly framed empty tar archive.
    assert!(archive_members(&[]).is_err());
    assert!(archive_members(&[0; 1024]).is_err());
    assert!(archive_members(b"truncated header").is_err());

    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_path("fixture").unwrap();
    header.set_size(4);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder.append(&header, &b"data"[..]).unwrap();
    let valid = builder.into_inner().unwrap();
    let members = archive_members(&valid).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].name, "fixture");
    assert_eq!(members[0].bytes, b"data");

    let mut invalid_checksum = valid.clone();
    invalid_checksum[0] ^= 1;
    assert!(archive_members(&invalid_checksum).is_err());
    // A valid header does not make an incomplete payload valid.
    assert!(archive_members(&valid[..514]).is_err());
}

#[test]
fn export_modes_and_timestamps_do_not_change_package_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let mut packages = Vec::new();
    for (mode, timestamp) in [(0o600, 1234), (0o755, 5678)] {
        let assets = directory.path().join(mode.to_string());
        fs::create_dir(&assets).unwrap();
        for name in NAMES {
            let path = assets.join(name);
            fs::write(&path, name.as_bytes()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp);
            File::open(path)
                .unwrap()
                .set_times(FileTimes::new().set_accessed(time).set_modified(time))
                .unwrap();
        }
        let output = directory.path().join(format!("{mode}.tar.gz"));
        package(&assets, &output);
        packages.push(output);
    }
    // This preserves the existing package normalization assertion; it is not
    // a compiled-engine reproducibility or source-provenance gate.
    assert_eq!(
        fs::read(&packages[0]).unwrap(),
        fs::read(&packages[1]).unwrap()
    );
    let members = package_members(&packages[0]);
    assert_expected_members(&members);
    for entry in members {
        assert_eq!(
            entry.mode,
            if matches!(entry.name.as_str(), "compose-init" | "mke2fs") {
                0o755
            } else {
                0o644
            },
            "{}",
            entry.name
        );
        assert_eq!(
            (entry.uid, entry.gid, entry.mtime),
            (0, 0, 0),
            "{}",
            entry.name
        );
        assert_eq!(entry.bytes, entry.name.as_bytes(), "{}", entry.name);
    }
}

struct ReadOnlyDirectory(PathBuf);
impl Drop for ReadOnlyDirectory {
    fn drop(&mut self) {
        fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[test]
fn package_accepts_read_only_nix_outputs() {
    let directory = tempfile::tempdir().unwrap();
    let guest = directory.path().join("guest");
    fs::create_dir(&guest).unwrap();
    for name in NAMES {
        let path = guest.join(name);
        fs::write(&path, name.as_bytes()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
    }
    fs::write(guest.join("runtime.json"), "obsolete hash manifest").unwrap();
    fs::write(guest.join("provenance.json"), "obsolete provenance").unwrap();
    let _restore_permissions = ReadOnlyDirectory(guest.clone());
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o555)).unwrap();
    let output = directory.path().join("runtime.tar.gz");
    package(&guest, &output);
    assert_eq!(
        fs::metadata(&guest).unwrap().permissions().mode() & 0o777,
        0o555
    );
    let members = package_members(&output);
    assert_expected_members(&members);
    for entry in members {
        assert_eq!(entry.bytes, entry.name.as_bytes(), "{}", entry.name);
    }
}
