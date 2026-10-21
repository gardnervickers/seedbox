#![cfg(unix)]
//! Actual ordinary-machine control transport, with a deliberately manual host
//! responder. These tests do not provide a simulation service or fault dispatcher.
mod support {
    pub mod prepared_application;
    pub mod simulation_control;
}
use support::simulation_control as control;

#[test]
#[ignore = "requires patched QEMU, Linux inputs and SEEDBOX_TCG_CONTROL_INITRD"]
fn tcg_native_control_round_trip_disconnect_and_application_restart() {
    control::round_trip(control::tcg());
}
#[test]
#[ignore = "requires patched QEMU, Linux inputs and SEEDBOX_TCG_CONTROL_INITRD"]
fn tcg_native_control_late_reply_after_reboot_and_crash_is_discarded() {
    control::late_reply_after_reset(control::tcg());
}
#[test]
#[ignore = "requires patched QEMU, Linux inputs and SEEDBOX_TCG_CONTROL_INITRD"]
fn tcg_native_control_pending_reply_snapshot_delivers_once() {
    control::pending_snapshot(control::tcg(), |snapshot| {
        seedbox_machine_tcg::machine::tcg::TcgMachine::from_snapshot(
            &control::input("SEEDBOX_TCG_QEMU"),
            snapshot,
        )
        .unwrap()
    });
}
