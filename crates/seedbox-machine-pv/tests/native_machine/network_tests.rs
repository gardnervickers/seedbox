//! Real Linux on native PV with one NIC per machine: the shared direct
//! `Machine` scenarios of `machine/network_scenario.rs` between two guests,
//! plain and with the server recording its progress on a disk.
use seedbox_machine::test_support::network_scenario::{self as scenario, Backend};
use seedbox_machine_pv::machine::pv::{Factory, PVMachine};
use std::path::PathBuf;

fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}

fn backend() -> Backend<PVMachine> {
    Backend {
        name: "pv",
        factory: || {
            let mut factory = Factory::new(
                &input("SEEDBOX_PV_WORKER"),
                &input("SEEDBOX_PV_ASSETS"),
                super::TEST_EPOCH_SECONDS,
            )
            .unwrap();
            Box::new(move |definition| factory.create(definition))
        },
        restore: |snapshot| {
            PVMachine::from_snapshot(&input("SEEDBOX_PV_WORKER"), snapshot)
                .map_err(std::io::Error::other)
        },
        probe: input("SEEDBOX_PV_NETWORK_APPLICATION"),
        memory_bytes: 128 << 20,
    }
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
fn pv_linux_guests_exchange_arp_udp_and_tcp_repeatably() {
    scenario::plain_exchange_is_repeatable(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
fn pv_snapshot_continuation_matches_the_uninterrupted_exchange() {
    scenario::plain_snapshot_continuation(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
fn pv_server_crash_and_reboot_drops_old_boot_frames_and_recovers() {
    scenario::server_crash_and_reboot(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
fn pv_disk_server_exchange_records_progress_repeatably() {
    scenario::disk_exchange_is_repeatable(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
fn pv_disk_server_snapshot_continuation_matches_the_uninterrupted_exchange() {
    scenario::disk_snapshot_continuation(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
fn pv_disk_server_crash_after_ready_keeps_flushed_records() {
    scenario::disk_server_crash_keeps_flushed_records(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
#[should_panic(expected = "probe exited 70: \"NETPROBE fail disk boot record errno=74")]
fn pv_disk_server_rejects_a_malformed_boot_record() {
    scenario::disk_rejects_a_malformed_boot_record(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
#[should_panic(expected = "probe exited 70: \"NETPROBE fail disk boot count errno=75")]
fn pv_disk_server_rejects_an_exhausted_boot_count() {
    scenario::disk_rejects_an_exhausted_boot_count(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and network fixtures"]
#[should_panic(
    expected = "0 probe exited 70: \"NETPROBE disk boot=1 token=none tcp=none\\nNETPROBE ready port=7000\\nNETPROBE fail disk token length errno=75\\n\""
)]
fn pv_disk_server_rejects_an_oversized_token() {
    scenario::disk_rejects_an_oversized_token(&backend());
}
