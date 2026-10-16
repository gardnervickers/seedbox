#![cfg(unix)]
//! Actual ordinary-machine control transport, with a deliberately manual host
//! responder. These tests do not provide a simulation service or fault dispatcher.
#[path = "support/simulation_control.rs"]
mod control;
#[path = "support/prepared_application.rs"]
mod prepared;

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
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_CONTROL_INITRD"]
fn pv_native_control_round_trip_disconnect_and_application_restart() {
    control::round_trip(control::pv());
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_CONTROL_INITRD"]
fn pv_native_control_late_reply_after_reboot_and_crash_is_discarded() {
    control::late_reply_after_reset(control::pv());
}
