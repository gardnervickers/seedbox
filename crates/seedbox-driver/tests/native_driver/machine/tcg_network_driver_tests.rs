//! Real Linux on the TCG reference machine through the actual Driver network.
//! Scenarios and assertions are shared (`machine/network_driver_scenario.rs`);
//! this entry only supplies the runner that builds TCG machines from its
//! inputs, as the PV entry does.
use crate::machine::network_driver_scenario::{self as scenario, run, Run, Scenario};
use seedbox_machine_tcg::machine::tcg::Factory;
use std::{fs, path::PathBuf};

fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}

fn tcg(scenario: &Scenario) -> Run {
    let probe = fs::read(input("SEEDBOX_TCG_NETWORK_APPLICATION")).unwrap();
    let mut factory = Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let result = run(&probe, 128 << 20, scenario, move |definition| {
        factory.create(definition)
    });
    eprintln!(
        "tcg driver run (workers {}): {} events, {} TCP retransmissions, client {:?}, outcome {:?}",
        scenario.workers,
        result.events.len(),
        result.tcp_retransmissions(),
        result.stdout[2],
        result.outcome
    );
    result
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and network fixtures"]
fn tcg_driver_network_exchange_is_identical_across_worker_counts() {
    scenario::clean_exchange(tcg);
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and network fixtures"]
fn tcg_driver_network_recovers_from_seeded_jitter_and_loss() {
    scenario::seeded_loss(tcg);
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and network fixtures"]
fn tcg_driver_partition_cuts_active_tcp_and_heal_recovers() {
    scenario::partition_and_heal(tcg);
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and network fixtures"]
fn tcg_driver_network_tolerates_opposite_ten_percent_drift() {
    scenario::opposite_drift(tcg);
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and network fixtures"]
fn tcg_driver_network_resumes_a_paused_server() {
    scenario::paused_server(tcg);
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and network fixtures"]
fn tcg_driver_network_survives_a_server_crash_and_reboot() {
    scenario::server_crash(tcg);
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and network fixtures"]
fn tcg_driver_disk_server_keeps_flushed_records_across_crash_and_completes_tcp() {
    scenario::disk_server_crash(tcg);
}
