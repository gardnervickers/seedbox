#![cfg(unix)]
//! Actual ordinary-machine control transport, with a deliberately manual host
//! responder. These tests do not provide a simulation service or fault dispatcher.
use seedbox_machine::test_support::{
    prepared_application as prepared, simulation_control as control, TEST_EPOCH_SECONDS,
};

fn tcg() -> seedbox_machine_tcg::machine::tcg::TcgMachine {
    let definition = prepared::definition(
        std::fs::read(control::input("SEEDBOX_TCG_CONTROL_INITRD")).unwrap(),
        256 << 20,
    );
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &control::input("SEEDBOX_TCG_QEMU"),
        &control::input("SEEDBOX_TCG_ASSETS"),
        TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}

#[test]
#[ignore = "requires patched QEMU, Linux inputs and SEEDBOX_TCG_CONTROL_INITRD"]
fn tcg_native_control_round_trip_disconnect_and_application_restart() {
    control::round_trip(tcg());
}
#[test]
#[ignore = "requires patched QEMU, Linux inputs and SEEDBOX_TCG_CONTROL_INITRD"]
fn tcg_native_control_late_reply_after_reboot_and_crash_is_discarded() {
    control::late_reply_after_reset(tcg());
}
#[test]
#[ignore = "requires patched QEMU, Linux inputs and SEEDBOX_TCG_CONTROL_INITRD"]
fn tcg_native_control_pending_reply_snapshot_delivers_once() {
    control::pending_snapshot(tcg(), |snapshot| {
        seedbox_machine_tcg::machine::tcg::TcgMachine::from_snapshot(
            &control::input("SEEDBOX_TCG_QEMU"),
            snapshot,
        )
        .unwrap()
    });
}
