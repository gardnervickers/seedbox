use super::*;
use std::{collections::BTreeSet, io::Write};
mod tar_headers;

fn elf(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes
}
fn files() -> Vec<(String, Vec<u8>, u32)> {
    let native = std::env::consts::ARCH;
    let mut files = vec![("host-system".into(), host_system().into_bytes(), 0o644)];
    for (backend, arch) in [("pv", native), ("tcg", "x86_64")] {
        for name in [
            "kernel",
            "init",
            "command-line",
            "mke2fs",
            if backend == "pv" {
                "progress.tar"
            } else {
                "firmware"
            },
        ] {
            let bytes = if name == "init" || name == "mke2fs" {
                elf(if arch == "x86_64" { 62 } else { 183 })
            } else if name == "command-line" {
                vec![]
            } else {
                name.as_bytes().to_owned()
            };
            files.push((
                format!("{backend}/{arch}/{name}"),
                bytes,
                if name == "mke2fs" || name == "init" {
                    0o755
                } else {
                    0o644
                },
            ));
        }
    }
    for name in [
        "bin/qemu-system-x86_64",
        "libexec/qemu-system-x86_64",
        "lib/loader",
        "share/qemu/qboot.rom",
    ] {
        files.push((
            format!("host/{}/{name}", host_system()),
            if name.starts_with("libexec/") {
                if cfg!(target_os = "linux") {
                    elf(if cfg!(target_arch = "x86_64") {
                        62
                    } else {
                        183
                    })
                } else {
                    let mut header = 0xfeed_facf_u32.to_le_bytes().to_vec();
                    header.extend_from_slice(
                        &(if cfg!(target_arch = "aarch64") {
                            0x0100_000c_u32
                        } else {
                            0x0100_0007_u32
                        })
                        .to_le_bytes(),
                    );
                    header
                }
            } else {
                name.as_bytes().to_owned()
            },
            if name.starts_with("bin/") || name.starts_with("libexec/") {
                0o755
            } else {
                0o644
            },
        ));
    }
    files
}
fn package(files: &[(String, Vec<u8>, u32)]) -> Vec<u8> {
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        vec![],
        flate2::Compression::fast(),
    ));
    for (name, bytes, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        archive.append_data(&mut header, name, &bytes[..]).unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}
fn native() -> Architecture {
    if cfg!(target_arch = "x86_64") {
        Architecture::X86_64
    } else {
        Architecture::Aarch64
    }
}

#[test]
fn default_pv_and_requested_x86_tcg_select_distinct_guest_directories() {
    let scratch = tempfile::tempdir().unwrap();
    assert_eq!(Backend::default(), Backend::Pv);
    let bytes = package(&files());
    let runtime = Runtime::from_bytes(&bytes, scratch.path()).unwrap();
    let pv = runtime.select(Backend::Pv, native()).unwrap();
    assert!(pv
        .guest_assets
        .ends_with(format!("pv/{}", std::env::consts::ARCH)));
    assert!(pv.qemu.is_none());
    assert!(pv.guest_assets.join("progress.tar").is_file());
    let tcg = runtime.select(Backend::Tcg, Architecture::X86_64).unwrap();
    assert!(tcg.guest_assets.ends_with("tcg/x86_64"));
    assert!(tcg.qemu.unwrap().is_file());
    assert!(runtime.select(Backend::Tcg, Architecture::Aarch64).is_err());
    let foreign = if native() == Architecture::X86_64 {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    };
    assert!(runtime.select(Backend::Pv, foreign).is_err());
}
#[test]
fn archive_and_entire_closure_survive_source_run_deletion_and_round_trip() {
    let scratch = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let store = ContentStore::new(store_dir.path());
    let path = source.path().join("runtime.tar.gz");
    let bytes = package(&files());
    fs::write(&path, &bytes).unwrap();
    let original = Runtime::from_archive(&path, scratch.path()).unwrap();
    let digest = original.retain(&store).unwrap();
    let inputs = original.select(Backend::Tcg, Architecture::X86_64).unwrap();
    let root = original.directory.path().to_owned();
    drop(original);
    drop(source);
    assert!(root.exists());
    assert_eq!(fs::read(inputs.archive()).unwrap(), bytes);
    drop(inputs);
    assert!(!root.exists());
    let restored = Runtime::from_store(&store, &digest, scratch.path()).unwrap();
    let inputs = restored.select(Backend::Tcg, Architecture::X86_64).unwrap();
    for (name, bytes, mode) in files() {
        let path = restored.directory.path().join("assets").join(name);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    assert!(inputs
        .qemu
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("lib/loader")
        .is_file());
}
#[test]
fn missing_formatter_wrong_guest_isa_or_missing_closure_never_falls_back() {
    let scratch = tempfile::tempdir().unwrap();
    for bad in ["formatter", "isa", "closure", "mode"] {
        let mut files = files();
        match bad {
            "formatter" => files.retain(|(name, _, _)| name != "tcg/x86_64/mke2fs"),
            "isa" => {
                files
                    .iter_mut()
                    .find(|(name, _, _)| name == "tcg/x86_64/init")
                    .unwrap()
                    .1 = elf(183)
            }
            "closure" => files.retain(|(name, _, _)| !name.contains("/lib/")),
            "mode" => {
                files
                    .iter_mut()
                    .find(|(name, _, _)| name.ends_with("/bin/qemu-system-x86_64"))
                    .unwrap()
                    .2 = 0o644
            }
            _ => unreachable!(),
        }
        let runtime = Runtime::from_bytes(&package(&files), scratch.path()).unwrap();
        assert!(
            runtime.select(Backend::Tcg, Architecture::X86_64).is_err(),
            "{bad}"
        );
    }
}
#[test]
fn duplicate_unrecognized_traversal_and_link_members_are_rejected() {
    let scratch = tempfile::tempdir().unwrap();
    let mut duplicate = files();
    duplicate.push(duplicate[0].clone());
    assert!(Runtime::from_bytes(&package(&duplicate), scratch.path()).is_err());
    for name in [
        "tcg/aarch64/kernel",
        "unexpected",
        "/absolute",
        "../escape",
        "pv/./kernel",
        "pv//kernel",
    ] {
        assert!(!allowed(name));
        if !name.starts_with('/') && !name.contains("..") {
            let mut f = files();
            f.push((name.into(), vec![1], 0o644));
            assert!(Runtime::from_bytes(&package(&f), scratch.path()).is_err());
        }
    }
    for kind in [
        tar::EntryType::Symlink,
        tar::EntryType::Link,
        tar::EntryType::Directory,
        tar::EntryType::Fifo,
    ] {
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            vec![],
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(kind);
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("/outside").unwrap();
        }
        archive
            .append_data(&mut header, "host-system", std::io::empty())
            .unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        assert!(Runtime::from_bytes(&bytes, scratch.path()).is_err());
    }
}
#[test]
fn unsupported_host_archive_and_oversized_payload_are_rejected_before_allocation() {
    let scratch = tempfile::tempdir().unwrap();
    let mut f = files();
    f[0].1 = b"not-the-build-host".to_vec();
    assert!(Runtime::from_bytes(&package(&f), scratch.path())
        .unwrap_err()
        .to_string()
        .contains("does not match"));
    let mut header = tar::Header::new_gnu();
    header.set_path("tcg/x86_64/kernel").unwrap();
    header.set_mode(0o644);
    header.set_size(UNPACKED_LIMIT + 1);
    header.set_cksum();
    let mut gzip = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
    gzip.write_all(header.as_bytes()).unwrap();
    assert!(Runtime::from_bytes(&gzip.finish().unwrap(), scratch.path())
        .unwrap_err()
        .to_string()
        .contains("exceeds 4 GiB"));
    let file = tempfile::NamedTempFile::new().unwrap();
    file.as_file().set_len(COMPRESSED_LIMIT + 1).unwrap();
    assert!(Runtime::from_archive(file.path(), scratch.path())
        .unwrap_err()
        .to_string()
        .contains("exceeds 1 GiB"));
}
#[test]
fn explicit_session_root_owns_every_extracted_file_and_failure_cleans_up() {
    let scratch = tempfile::tempdir().unwrap();
    let bytes = package(&files());
    let runtime = Runtime::from_bytes(&bytes, scratch.path()).unwrap();
    assert!(runtime.archive().starts_with(scratch.path()));
    assert!(runtime
        .select(Backend::Tcg, Architecture::X86_64)
        .unwrap()
        .qemu
        .unwrap()
        .starts_with(scratch.path()));
    drop(runtime);
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    assert!(Runtime::from_bytes(b"invalid gzip", scratch.path()).is_err());
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    let missing = scratch.path().join("caller-must-create-this");
    let error = Runtime::from_bytes(&bytes, &missing)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&missing.display().to_string()));
    assert!(error.contains("existing writable session directory"));
    assert!(!missing.exists());
}

#[test]
fn gzip_trailer_checksums_and_hidden_suffix_data_are_not_ignored() {
    let scratch = tempfile::tempdir().unwrap();
    let bytes = package(&files());
    let mut corrupted = bytes.clone();
    let length = corrupted.len();
    corrupted[length - 5] ^= 1;
    assert!(Runtime::from_bytes(&corrupted, scratch.path()).is_err());
    assert!(Runtime::from_bytes(&bytes[..bytes.len() - 2], scratch.path()).is_err());
    let mut suffix = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
    suffix.write_all(b"unowned suffix").unwrap();
    let mut extended = bytes;
    extended.extend(suffix.finish().unwrap());
    assert!(Runtime::from_bytes(&extended, scratch.path()).is_err());
}

#[test]
fn concurrent_loads_own_independent_paths_and_do_not_publish_partial_inputs() {
    let scratch = tempfile::tempdir().unwrap();
    let bytes = package(&files());
    std::thread::scope(|scope| {
        let children: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let runtime = Runtime::from_bytes(&bytes, scratch.path()).unwrap();
                    let inputs = runtime.select(Backend::Tcg, Architecture::X86_64).unwrap();
                    (runtime, inputs)
                })
            })
            .collect();
        let loaded: Vec<_> = children
            .into_iter()
            .map(|child| child.join().unwrap())
            .collect();
        let roots: BTreeSet<_> = loaded
            .iter()
            .map(|(runtime, _)| runtime.directory.path())
            .collect();
        assert_eq!(roots.len(), 4);
    });
    assert!(Runtime::from_bytes(&bytes[..bytes.len() / 2], scratch.path()).is_err());
}
