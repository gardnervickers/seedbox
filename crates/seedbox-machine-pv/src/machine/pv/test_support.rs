use super::{
    test_image::{program, Op},
    PVMachine,
};
use std::{io, path::Path};
/// Configure original native boot blobs, preserving deferred resource creation.
pub fn boot(worker: &Path, kernel: Vec<u8>, initrd: Vec<u8>) -> io::Result<PVMachine> {
    PVMachine::from_boot_inputs(worker, kernel, initrd)
}
/// Original timed instruction fixture, assembled without altering its operations.
pub fn timed_guest() -> Vec<u8> {
    super::test_image::timed_guest()
}
/// Original horizon fixture, assembled without altering its operations.
pub fn horizon_guest() -> Vec<u8> {
    program(&[Op::Ready, Op::Yield, Op::PowerOff])
}
/// Original raw Linux smoke initramfs with its existing launch behavior.
pub fn linux_initrd() -> Vec<u8> {
    super::test_image::linux_initrd()
}
/// Preserve the native Driver smoke test's stopped console assertion locally.
pub fn assert_linux_pass_console(machine: &PVMachine) {
    let console = String::from_utf8_lossy(&machine.console);
    assert!(console.contains("PV_LINUX_PASS"), "{console}");
}
