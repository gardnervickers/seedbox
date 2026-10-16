use seedbox_driver::driver;
pub use seedbox_machine::machine::*;
pub(crate) const TEST_EPOCH_SECONDS: u32 = 946_684_800;
#[cfg(unix)]
pub(crate) use seedbox_machine::test_support::{disk_scenario, network_scenario};
#[cfg(seedbox_pv)]
pub(crate) use seedbox_machine_pv::machine::pv;
#[cfg(unix)]
pub(crate) use seedbox_machine_tcg::machine::tcg;
#[cfg(unix)]
mod clock_fault_tests;
#[cfg(unix)]
mod disk_driver_scenario;
#[cfg(unix)]
mod driver_input_snapshot_tests;
#[cfg(unix)]
mod driver_sleep_trigger_tests;
#[cfg(unix)]
mod guest_service_snapshot_tests;
#[cfg(unix)]
mod kernel_driver_tests;
#[cfg(unix)]
#[allow(dead_code)]
mod network_driver_scenario;
#[cfg(seedbox_pv)]
mod pv_disk_driver_tests;
#[cfg(seedbox_pv)]
mod pv_network_driver_tests;
#[cfg(unix)]
mod tcg_disk_driver_tests;
#[cfg(unix)]
mod tcg_network_driver_tests;

fn auto_key() -> driver::InputKey {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    driver::InputKey::new(format!("auto-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap()
}
fn settlements(
    events: &[driver::Event],
) -> Vec<(
    driver::FaultRecord,
    driver::SettlementStatus,
    Option<Timestamp>,
    Timestamp,
)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            driver::EventKind::FaultSettled {
                record,
                status,
                outcome_at,
                ..
            } => Some((record.clone(), *status, *outcome_at, event.at)),
            _ => None,
        })
        .collect()
}
