//! Real Linux on native PV with one attached disk: the shared scenarios of
//! `machine/disk_scenario.rs`.
use crate::machine::pv::{Factory, PVMachine};
use seedbox_machine::test_support::disk_scenario::{self, Backend};
use std::path::PathBuf;

fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}

fn backend() -> Backend<PVMachine> {
    Backend {
        factory: || {
            let mut factory = Factory::new(
                &input("SEEDBOX_PV_WORKER"),
                &input("SEEDBOX_PV_ASSETS"),
                crate::machine::TEST_EPOCH_SECONDS,
            )
            .unwrap();
            Box::new(move |definition| factory.create(definition))
        },
        restore: |snapshot| {
            PVMachine::from_snapshot(&input("SEEDBOX_PV_WORKER"), snapshot)
                .map_err(std::io::Error::other)
        },
        store: PVMachine::disk_store,
        probe: input("SEEDBOX_PV_DISK_PROBE"),
        image: input("SEEDBOX_PV_DISK_IMAGE"),
        memory_bytes: 128 << 20,
    }
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and disk fixtures"]
fn pv_disk_raw_flush_survives_boot_end_and_unflushed_writes_are_lost() {
    disk_scenario::raw_flush_survives_boot_end_and_unflushed_writes_are_lost(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and disk fixtures"]
fn pv_disk_ext4_fsync_survives_crash_and_shutdown_sync_keeps_dirty_data() {
    disk_scenario::ext4_fsync_survives_crash_and_shutdown_sync_keeps_dirty_data(&backend());
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and disk fixtures"]
fn pv_disk_snapshots_keep_both_layers_independently_without_promotion() {
    disk_scenario::snapshots_keep_both_layers_independently_without_promotion(&backend());
}
