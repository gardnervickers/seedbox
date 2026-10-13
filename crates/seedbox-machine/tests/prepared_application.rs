#![cfg(unix)]
//! Public factory and retained image launch acceptance.
use seedbox_machine as machine_crate;
#[path = "support/prepared_image.rs"]
mod image;
use image::*;
use std::fs;
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
fn pv(script: bool) {
    let mut backend = machine_crate::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
    )
    .unwrap();
    let binary = fs::read(input("SEEDBOX_PV_PREPARED_FIXTURE")).unwrap();
    check(
        backend.create(&definition(&binary, script)).unwrap(),
        script,
        false,
    );
}
fn tcg(script: bool) {
    let mut backend = machine_crate::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
    )
    .unwrap();
    let binary = fs::read(input("SEEDBOX_TCG_PREPARED_FIXTURE")).unwrap();
    check(
        backend.create(&definition(&binary, script)).unwrap(),
        script,
        false,
    );
}
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
#[test]
#[ignore = "requires native PV worker, factory assets and prepared ELF fixture"]
fn pv_native_prepared_image_elf_launch() {
    pv(false);
}
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
#[test]
#[ignore = "requires native PV worker, factory assets and prepared script fixture"]
fn pv_native_prepared_image_script_launch() {
    pv(true);
}
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
#[test]
#[ignore = "requires native PV worker, factory assets and prepared ELF fixture"]
fn pv_native_prepared_image_readable_elf_launch() {
    let mut backend = machine_crate::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
    )
    .unwrap();
    let binary = fs::read(input("SEEDBOX_PV_PREPARED_FIXTURE")).unwrap();
    let mut workload = definition(&binary, false);
    // A separate control isolates DR's launch/argv behavior from its ability
    // to load execute-only bytes. The original 0111 scenario is unchanged.
    workload.application.filesystem = archive_mode(&binary, false, 0o755);
    check(backend.create(&workload).unwrap(), false, false);
}
#[test]
#[ignore = "requires native TCG QEMU, factory assets and prepared ELF fixture"]
fn tcg_native_prepared_image_elf_launch() {
    tcg(false);
}
#[test]
#[ignore = "requires native TCG QEMU, factory assets and prepared script fixture"]
fn tcg_native_prepared_image_script_launch() {
    tcg(true);
}

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
#[test]
#[ignore = "requires native PV worker, factory assets and prepared ELF fixture"]
fn pv_native_runtime_respects_execute_traversal_and_uid_authorization() {
    let mut backend = machine_crate::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
    )
    .unwrap();
    let binary = fs::read(input("SEEDBOX_PV_PREPARED_FIXTURE")).unwrap();
    for (program_mode, directory_mode, user, error) in [
        (0o644, 0o755, "1234:2345", "errno 13"),
        (0o755, 0o700, "1234:2345", "errno 13"),
        (0o111, 0o755, "4294967295:2345", "errno 22"),
    ] {
        let mut workload = definition(&binary, false);
        workload.application.filesystem =
            archive_with_modes(&binary, false, program_mode, directory_mode);
        workload.application.launch.user = user.into();
        check_denied(backend.create(&workload).unwrap(), error);
    }
}
