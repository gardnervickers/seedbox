#![cfg(seedbox_pv)]
//! Native guest service acceptance through this backend's public Machine factory.
#[path = "support/epoch.rs"]
mod epoch;
#[path = "support/prepared_application.rs"]
mod prepared;
use seedbox_machine::test_support::guest_services::{applications, shutdown};
use std::{fs, path::PathBuf};
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}
#[cfg(seedbox_pv)]
fn pv() -> seedbox_machine_pv::machine::pv::PVMachine {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        128 << 20,
    );
    let mut backend = seedbox_machine_pv::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_SERVICE_INITRD static helper+fixture"]
fn pv_native_application_controls_logs_and_outcome() {
    applications(pv());
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_SERVICE_INITRD static helper+fixture"]
fn pv_native_shutdown_preserves_graceful_cleanup() {
    shutdown(pv());
}
