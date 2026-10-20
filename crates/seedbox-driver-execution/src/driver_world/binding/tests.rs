//! Structural package ownership tests only. These headers contain no runnable
//! guest or host program, and no machine/factory/worker is constructed.
use super::*;
use crate::runtime_assets::Runtime;
use seedbox_spec::backend::CpuBackend;
use seedbox_spec::definition::Architecture;
use std::io::Write;

fn package(kernel: &[u8]) -> Vec<u8> {
    let arch = std::env::consts::ARCH;
    let system = format!(
        "{arch}-{}",
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    );
    let mut elf = vec![0; 20];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    elf[18..20].copy_from_slice(
        &(if cfg!(target_arch = "aarch64") {
            183_u16
        } else {
            62_u16
        })
        .to_le_bytes(),
    );
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, bytes, mode) in [
        ("host-system".to_owned(), system.as_bytes(), 0o444),
        (format!("pv/{arch}/kernel"), kernel, 0o444),
        (format!("pv/{arch}/init"), elf.as_slice(), 0o555),
        (format!("pv/{arch}/mke2fs"), elf.as_slice(), 0o555),
        (format!("pv/{arch}/command-line"), b"".as_slice(), 0o444),
        (
            format!("pv/{arch}/progress.tar"),
            b"structural package fixture".as_slice(),
            0o444,
        ),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive.append_data(&mut header, name, bytes).unwrap();
    }
    let mut gzip = archive.into_inner().unwrap();
    gzip.flush().unwrap();
    gzip.finish().unwrap()
}
fn inputs(runtime: &Runtime) -> Inputs {
    runtime
        .select(
            CpuBackend::Pv,
            if cfg!(target_arch = "aarch64") {
                Architecture::Aarch64
            } else {
                Architecture::X86_64
            },
        )
        .unwrap()
}
#[test]
fn matching_inputs_share_parent_package_and_guards_cover_each_executor() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::from_bytes(&package(b"structural kernel"), root.path()).unwrap();
    let inputs = inputs(&runtime);
    let archive = inputs.archive();
    let first = Binding::new(inputs.clone()).unwrap();
    let second = Binding::new(inputs.clone()).unwrap();
    drop(inputs);
    drop(runtime);
    assert_eq!(first.wire, second.wire);
    assert_eq!(first.files, second.files);
    assert_eq!(
        fs::read_dir(root.path()).unwrap().count(),
        1,
        "matching Inputs must not extract per executor"
    );
    drop(first);
    assert!(
        archive.exists(),
        "other executor retains the complete package"
    );
    drop(second);
    assert!(!archive.exists());
}
#[test]
fn distinct_retained_archives_remain_independent_in_one_owned_session() {
    let root = tempfile::tempdir().unwrap();
    let first_runtime =
        Runtime::from_bytes(&package(b"first retained kernel"), root.path()).unwrap();
    let second_runtime =
        Runtime::from_bytes(&package(b"different retained kernel"), root.path()).unwrap();
    let first = Binding::new(inputs(&first_runtime)).unwrap();
    let second = Binding::new(inputs(&second_runtime)).unwrap();
    assert_ne!(first.wire.archive.sha256, second.wire.archive.sha256);
    assert_ne!(first.wire.archive.artifact, second.wire.archive.artifact);
    first.wire.validate_archive(&first.wire.archive).unwrap();
    let error = first
        .wire
        .validate_archive(&second.wire.archive)
        .unwrap_err();
    assert!(error.contains("retained factory archive"));
    assert!(error.contains("rather than the startup/default runtime"));
    second.wire.validate_archive(&second.wire.archive).unwrap();
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    drop(first_runtime);
    drop(second_runtime);
    drop(first);
    assert!(Path::new(&second.wire.archive.artifact).exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
