//! Opt-in synthetic runtime package for dependents' preparation tests.
// Feature-gated rather than cfg(test): other crates' tests use it.
use std::{fs, path::Path};

pub fn sdk(
    root: &Path,
    backend: seedbox_spec::backend::CpuBackend,
) -> (
    crate::runtime_assets::Runtime,
    crate::runtime_assets::Inputs,
    std::path::PathBuf,
) {
    use crate::runtime_assets;
    let package = root.join("package.tar.gz");
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        fs::File::create(&package).unwrap(),
        flate2::Compression::fast(),
    ));
    let mut append = |name: &str, bytes: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(if name.contains("/bin/") || name.contains("/libexec/") {
            0o755
        } else {
            0o644
        });
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes).unwrap();
    };
    let system = format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    );
    append("host-system", system.as_bytes());
    let mut kernel = seedbox_machine::test_support::assets::bzimage_header();
    kernel[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
    let mut arm_kernel = vec![0; 65536];
    arm_kernel[8..16].copy_from_slice(&0x80000_u64.to_le_bytes());
    arm_kernel[16..24].copy_from_slice(&65536_u64.to_le_bytes());
    arm_kernel[56..60].copy_from_slice(b"ARM\x64");
    let arm_elf = seedbox_machine::test_support::assets::image(183);
    append("pv/aarch64/kernel", &arm_kernel);
    append("pv/aarch64/init", &arm_elf);
    append("pv/aarch64/mke2fs", &arm_elf);
    append("pv/aarch64/command-line", b"rdinit=/init");
    append("pv/x86_64/kernel", &kernel);
    let elf = seedbox_machine::test_support::assets::image(62);
    append("pv/x86_64/init", &elf);
    append("pv/x86_64/mke2fs", &elf);
    append("pv/x86_64/command-line", b"rdinit=/init");
    append("tcg/x86_64/kernel", &kernel);
    append("tcg/x86_64/init", &elf);
    append("tcg/x86_64/mke2fs", &elf);
    append("tcg/x86_64/command-line", b"rdinit=/init");
    append("tcg/x86_64/firmware", &vec![3; 64 * 1024]);
    // Host QEMU members live under the system recorded in host-system.
    let host = |name: &str| format!("host/{system}/{name}");
    append(&host("bin/qemu-system-x86_64"), b"#!/bin/sh\nexit 0\n");
    append(&host("libexec/qemu-system-x86_64"), &elf);
    append(&host("lib/loader"), b"cold library");
    append(&host("share/qemu/qboot.rom"), b"cold firmware");
    let mut progress = tar::Builder::new(Vec::new());
    for name in [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        "lib/ld-linux-x86-64.so.2",
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ] {
        let bytes = if name.ends_with(".txt") || name.ends_with(".LIB") {
            b"cold license".as_slice()
        } else {
            elf.as_slice()
        };
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        progress
            .append_data(&mut header, format!("seedbox/progress/{name}"), bytes)
            .unwrap();
    }
    let progress = progress.into_inner().unwrap();
    append("pv/x86_64/progress.tar", &progress);
    let mut arm_progress = Vec::new();
    let mut builder = tar::Builder::new(&mut arm_progress);
    for entry in tar::Archive::new(progress.as_slice()).entries().unwrap() {
        let mut entry = entry.unwrap();
        let mut name = entry.path().unwrap().into_owned();
        // The aarch64 runtime carries its own ELF loader name.
        if name.ends_with("lib/ld-linux-x86-64.so.2") {
            name = "seedbox/progress/lib/ld-linux-aarch64.so.1".into();
        }
        let bytes = if name.to_string_lossy().ends_with(".txt")
            || name.to_string_lossy().ends_with(".LIB")
        {
            b"cold license".as_slice()
        } else {
            arm_elf.as_slice()
        };
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).unwrap();
        std::io::copy(&mut entry, &mut std::io::sink()).unwrap();
    }
    builder.finish().unwrap();
    drop(builder);
    append("pv/aarch64/progress.tar", &arm_progress);
    tar.into_inner().unwrap().finish().unwrap();
    let runtime = runtime_assets::Runtime::from_archive(&package, root).unwrap();
    let selected = runtime
        .select(
            backend,
            if backend == seedbox_spec::backend::CpuBackend::Pv && cfg!(target_arch = "aarch64") {
                seedbox_spec::definition::Architecture::Aarch64
            } else {
                seedbox_spec::definition::Architecture::X86_64
            },
        )
        .unwrap();
    (runtime, selected, package)
}
