use super::{Boot, Error, Manifest, Snapshot, State, TcgMachine};
use seedbox_machine::machine::Timestamp;
use std::path::Path;
/// x86 raw-program fixtures: a 64 KiB BIOS image builder whose fixed stub
/// enters 32-bit flat protected mode, a minimal assembler, and the shared
/// register indexes, addresses and programs. Unit and integration tests use
/// this one builder.
pub mod raw_program {
    pub use super::super::raw_program_data::*;
}
/// Construct a raw program machine from a complete 64 KiB BIOS image, such as
/// one built by [`raw_program`], without adapting or correcting its bytes.
/// Existing length/resource validation and deferred startup remain unchanged.
pub fn program(qemu: &Path, bytes: Vec<u8>, memory_mib: u32) -> Result<TcgMachine, Error> {
    TcgMachine::new_raw(
        qemu,
        Manifest {
            boot: Boot::Program(bytes.into()),
            memory_mib,
            disk: None,
            network: None,
        },
    )
}
/// Construct the original explicit Linux blob fixture without changing inputs.
/// The private constructor validates firmware/kernel/memory before worker startup.
#[allow(clippy::too_many_arguments)]
pub fn linux(
    qemu: &Path,
    kernel: Vec<u8>,
    initramfs: Vec<u8>,
    firmware: Vec<u8>,
    command_line: String,
    epoch_seconds: u32,
    memory_mib: u32,
) -> Result<TcgMachine, Error> {
    TcgMachine::new_raw(
        qemu,
        Manifest {
            boot: Boot::Linux {
                kernel: kernel.into(),
                initramfs: initramfs.into(),
                firmware: firmware.into(),
                command_line,
                epoch_seconds,
            },
            memory_mib,
            disk: None,
            network: None,
        },
    )
}
/// Check the deferred construction invariant before Driver execution.
pub fn assert_unstarted(machine: &TcgMachine) {
    assert_eq!(machine.pending_boots, 1);
    assert!(machine.process.is_none());
}
/// Preserve an exact stopped execution-position assertion.
pub fn assert_reached(machine: &TcgMachine, at: Timestamp) {
    assert_eq!(machine.reached, at);
}
/// Preserve the stopped position ordering assertion against the Driver barrier.
pub fn assert_not_beyond(machine: &TcgMachine, now: Timestamp) {
    assert!(machine.reached <= now);
}
/// Preserve the stopped Ready assertion without exposing the state enum.
pub fn assert_ready(machine: &TcgMachine) {
    assert_eq!(machine.state, State::Ready);
}
/// Preserve the original matches!-form Ready assertion.
pub fn assert_matches_ready(machine: &TcgMachine) {
    assert!(matches!(machine.state, State::Ready));
}
/// Preserve the accepted pending-boot count assertion at its original barrier.
pub fn assert_no_pending_boots(machine: &TcgMachine) {
    assert_eq!(machine.pending_boots, 0);
}
/// The captured execution position used to construct the original continuation.
// TODO: Re-evaluate: this scenario value configures Driver and caller clocks.
// A boolean assertion cannot supply that existing restore input.
pub fn snapshot_reached(snapshot: &Snapshot) -> Timestamp {
    snapshot.reached
}
/// Read x86 register evidence while stopped; never writes or drives execution.
/// The result is the existing 17-register layout, not a fixture conversion.
pub fn registers(machine: &mut TcgMachine) -> Result<[u64; 17], Error> {
    machine.process()?.registers()
}

/// Preserve an exact captured stopped-position assertion without exposing state.
pub fn assert_snapshot_reached(snapshot: &Snapshot, at: Timestamp) {
    assert_eq!(snapshot.reached, at);
}
/// Preserve the original captured Ready assertion without exposing its enum.
pub fn assert_snapshot_ready(snapshot: &Snapshot) {
    assert_eq!(snapshot.state, State::Ready);
}
/// Preserve the original absence-of-pending-time assertion after admission.
pub fn assert_no_pending_at(machine: &TcgMachine) {
    assert_eq!(machine.pending_at, None);
}

/// Construct the shared shutdown program: an x86 BIOS image whose first
/// instruction after the fixed protected-mode stub (12 instructions) powers
/// the board off, with unchanged resource validation.
pub fn shutdown(qemu: &Path) -> Result<TcgMachine, Error> {
    program(qemu, raw_program::bios(&raw_program::SHUTDOWN), 16)
}
