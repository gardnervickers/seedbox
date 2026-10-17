//! Inspect the actual saved Driver authority with opaque native references.
//! This is NOT a native restore: no native body is decoded and run must never
//! be called. The surrounding case separately restores and runs real machines.
use super::*;
use seedbox_driver::driver::{self, Checkpoint, DecodeLimits, Driver, SnapshotReference};
use seedbox_machine::machine::{Machine, RunContext, StopReason, Timestamp};
use std::convert::Infallible;

struct OpaqueReference(SnapshotReference);
impl Machine for OpaqueReference {
    type Context = ();
    type Error = Infallible;
    type Snapshot = SnapshotReference;
    fn run(&mut self, _: &mut RunContext<'_, ()>, _: Timestamp) -> Result<StopReason, Infallible> {
        panic!("saved-authority inspection must never execute machine work")
    }
    fn snapshot(&mut self) -> Result<SnapshotReference, Infallible> {
        Ok(self.0)
    }
}

pub(super) fn assert_saved_pending(saved: &wire::Checkpoint, fault_id: u32) {
    let bytes = fs::read(&saved.driver.artifact).unwrap();
    assert_eq!(bytes.len() as u64, saved.driver.length);
    assert_eq!(ContentDigest::of(&bytes).hex(), saved.driver.sha256);
    let size = bytes.len() as u64;
    let limits = DecodeLimits {
        encoded_bytes: size,
        allocation_bytes: size.checked_mul(16).unwrap(),
        collection_items: size,
        total_items: size.checked_mul(16).unwrap(),
        string_bytes: size,
        blob_bytes: size,
        depth: 128,
        archive_entries: size,
        archive_path_bytes: size,
        archive_expanded_bytes: size,
    };
    // The closed codec validates the held reference against the exact pending
    // record/plan/FIFO and input ownership, before any machine restore callback.
    let checkpoint = Checkpoint::read_from(&mut &bytes[..], limits).unwrap();
    let mut driver = Driver::<OpaqueReference>::from_checkpoint(
        &checkpoint,
        1,
        || Ok(()),
        |reference| Ok(OpaqueReference(*reference)),
        |_| panic!("saved-authority inspection must not create a machine"),
    )
    .unwrap();
    let status = driver
        .with_barrier(|barrier| Ok(barrier.playback()))
        .unwrap();
    assert_eq!(status.rate, driver::PlaybackRate::Paused);
    assert!(!status.stepping && !status.stopped);
    assert_eq!(
        status.position.at.as_nanos(),
        saved.state.world.position.simulated_time_ns
    );
    assert_eq!(
        status.position.boundary_index,
        saved.state.world.position.boundary_index
    );
    assert_eq!(
        status.position.completed_quanta,
        saved.state.world.position.completed_rounds
    );
    let held = status
        .before_fault
        .expect("saved authority lost its unapplied held fault");
    assert_eq!(held.fault.get(), fault_id);
    assert!(matches!(&held.origin, driver::FaultOrigin::Input { key } if key.as_str() == KEY));
    assert!(
        matches!(held.action, driver::FaultAction::SetExecution(config) if config.factor_permille() == 2000)
    );
    assert!(
        matches!(held.target, driver::FaultTarget::Machine { machine, .. } if machine == driver::MachineId::ORACLE)
    );
    let driver::StepOutcome::BeforeFault { at, fault } = driver.step(status.position.at).unwrap()
    else {
        panic!("saved authority no longer stops before its admitted effect")
    };
    assert_eq!(at, status.position.at);
    assert_eq!(fault, held);
    // A real before-effect probe changes no authority byte, including pending
    // ownership and the held record. It must neither settle nor release it.
    let mut roundtrip = Vec::new();
    driver
        .checkpoint()
        .unwrap()
        .write_to(&mut roundtrip)
        .unwrap();
    assert_eq!(
        roundtrip, bytes,
        "inspecting held authority executed or settled work"
    );
}
