#![allow(
    clippy::disallowed_methods,
    reason = "test code: spawns here never race a Seedbox process writing an executable"
)]
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
pub(super) fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}
pub(super) fn initrd() -> Vec<u8> {
    initrd_source(
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../seedbox-machine/tests/fixtures/pv-lifecycle-init.c"),
        )
        .unwrap(),
    )
}
/// Compile a freestanding static PID 1 for the host's guest architecture.
/// Sources may include `pv-syscall.h` for portable generic syscall numbers.
pub(super) fn initrd_source(source: String) -> Vec<u8> {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("init");
    let source_path = directory.path().join("init.c");
    fs::write(&source_path, source).unwrap();
    // The x86 process entry stack is not call-aligned: realign every frame.
    let target: &[&str] = if cfg!(target_arch = "x86_64") {
        &["--target=x86_64-linux-gnu", "-mstackrealign"]
    } else {
        &["--target=aarch64-linux-gnu"]
    };
    let status = Command::new(input("SEEDBOX_PV_CLANG"))
        .args(target)
        .args([
            "-I",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../seedbox-machine/tests/fixtures"
            ),
            "-nostdlib",
            "-static",
            "-fno-stack-protector",
            "-O2",
            "-Wl,-e,_start",
        ])
        .arg(format!("-fuse-ld={}", input("SEEDBOX_PV_LD").display()))
        .arg(&source_path)
        .arg("-o")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());
    let program = fs::read(output).unwrap();
    let mut archive = Vec::new();
    for (name, mode, data, major, minor) in [
        ("dev", 0o040755, &[][..], 0, 0),
        ("sys", 0o040755, &[][..], 0, 0),
        ("dev/console", 0o020600, &[][..], 5, 1),
        ("init", 0o100755, &program[..], 0, 0),
        ("TRAILER!!!", 0, &[][..], 0, 0),
    ] {
        archive.extend(b"070701");
        for field in [
            1,
            mode,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            major,
            minor,
            name.len() + 1,
            0,
        ] {
            archive.extend(format!("{field:08x}").bytes());
        }
        archive.extend(name.bytes());
        archive.push(0);
        while archive.len() % 4 != 0 {
            archive.push(0);
        }
        archive.extend(data);
        while archive.len() % 4 != 0 {
            archive.push(0);
        }
    }
    archive
}
