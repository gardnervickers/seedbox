//! PV restore is intentionally private; public control-channel behavior and the
//! generic snapshot scenario are shared with integration tests.
use crate as machine_crate;
#[path = "../../tests/support/simulation_control.rs"]
mod control;

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
#[test]
#[ignore = "requires arm64 hypervisor, worker/kernel and SEEDBOX_PV_CONTROL_INITRD"]
fn pv_native_control_pending_reply_snapshot_delivers_once() {
    control::pending_snapshot(control::pv(), |snapshot| {
        super::pv::PVMachine::from_snapshot(&control::input("SEEDBOX_PV_WORKER"), snapshot).unwrap()
    });
}
