//! Hardware register restore semantics and guest entropy after checkpointing
//! require KVM execution; pure normalization has unit coverage in the backend.
#[path = "x86_checkpoints.rs"]
mod checkpoints;
#[path = "x86_machine.rs"]
mod machine;
#[path = "x86_xsave.rs"]
mod xsave;

#[test]
#[ignore = "requires Linux x86-64 KVM"]
fn native() {
    let _kernel = super::support::kernel();
    let kvm = kvm_ioctls::Kvm::new().unwrap();
    xsave::ymm_presence_changes_storage_but_only_nonzero_values_change_guest_registers(&kvm);
    machine::event_idx_polls_on_entry_slice_boundary_and_halt_without_kicks(&kvm);
    machine::page_timer_and_watchdog(&kvm);
    machine::progress_debt_stops_execution_and_survives_restore(&kvm);
    machine::changing_idle_periods_and_clock_adoption(&kvm);
    machine::checkpoint_and_interrupted_compute(&kvm);
}

#[test]
#[ignore = "requires KVM and fixture guest inputs"]
fn guests() {
    let _kernel = super::support::kernel();
    checkpoints::runtime_checkpoint_getrandom_and_urandom_repeat_and_diverge();
}
