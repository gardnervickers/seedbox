#![cfg(unix)]
//! Public factory and retained image launch acceptance.
#[path = "support/epoch.rs"]
mod epoch;
#[path = "support/prepared_image.rs"]
mod image;
use image::*;
use std::fs;
fn tcg(script: bool) {
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let binary = fs::read(input("SEEDBOX_TCG_PREPARED_FIXTURE")).unwrap();
    check(
        backend.create(&definition(&binary, script)).unwrap(),
        script,
        false,
    );
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

#[test]
#[ignore = "requires native TCG QEMU, factory assets and prepared ELF fixture"]
fn tcg_native_runtime_respects_execute_traversal_and_uid_authorization() {
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let binary = fs::read(input("SEEDBOX_TCG_PREPARED_FIXTURE")).unwrap();
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
