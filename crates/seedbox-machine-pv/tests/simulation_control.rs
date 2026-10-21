#![cfg(seedbox_pv)]
//! Actual ordinary-machine control transport, with a deliberately manual host
//! responder. These tests do not provide a simulation service or fault dispatcher.
use seedbox_machine::test_support::{
    prepared_application as prepared, simulation_control as control, TEST_EPOCH_SECONDS,
};

fn pv() -> seedbox_machine_pv::machine::pv::PVMachine {
    let definition = prepared::definition(
        std::fs::read(control::input("SEEDBOX_PV_CONTROL_INITRD")).unwrap(),
        128 << 20,
    );
    let mut backend = seedbox_machine_pv::machine::pv::Factory::new(
        &control::input("SEEDBOX_PV_WORKER"),
        &control::input("SEEDBOX_PV_ASSETS"),
        TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}

#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_CONTROL_INITRD"]
fn pv_native_control_round_trip_disconnect_and_application_restart() {
    control::round_trip(pv());
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_CONTROL_INITRD"]
fn pv_native_control_late_reply_after_reboot_and_crash_is_discarded() {
    control::late_reply_after_reset(pv());
}
