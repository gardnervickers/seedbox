#![cfg(unix)]
//! Native guest service acceptance through this backend's public Machine factory.
use seedbox_machine::test_support::guest_services::{applications, shutdown};
use seedbox_machine::test_support::prepared_application as prepared;
use seedbox_machine::test_support::TEST_EPOCH_SECONDS;
use std::{fs, path::PathBuf};
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}
fn tcg() -> seedbox_machine_tcg::machine::tcg::TcgMachine {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        256 << 20,
    );
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}
#[test]
#[ignore = "requires patched QEMU and SEEDBOX_TCG_SERVICE_INITRD static Linux helper+fixture"]
fn tcg_native_application_controls_logs_and_outcome() {
    applications(tcg());
}
#[test]
#[ignore = "requires patched QEMU and SEEDBOX_TCG_SERVICE_INITRD static Linux helper+fixture"]
fn tcg_native_shutdown_preserves_graceful_cleanup() {
    shutdown(tcg());
}
