//! Real Linux on the TCG reference machine with one attached disk: the
//! shared scenarios of `machine/disk_scenario.rs`, as on native PV.
use crate::machine::tcg::{Factory, TcgMachine};
use seedbox_machine::test_support::disk_scenario::{self, Backend};
use std::path::PathBuf;

fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}

fn backend() -> Backend<TcgMachine> {
    Backend {
        factory: || {
            let mut factory = Factory::new(
                &input("SEEDBOX_TCG_QEMU"),
                &input("SEEDBOX_TCG_ASSETS"),
                crate::machine::TEST_EPOCH_SECONDS,
            )
            .unwrap();
            Box::new(move |definition| factory.create(definition))
        },
        restore: |snapshot| TcgMachine::from_snapshot(&input("SEEDBOX_TCG_QEMU"), snapshot),
        store: TcgMachine::disk_store,
        probe: input("SEEDBOX_TCG_DISK_PROBE"),
        image: input("SEEDBOX_TCG_DISK_IMAGE"),
        memory_bytes: 128 << 20,
    }
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and disk fixtures"]
fn tcg_disk_raw_flush_survives_boot_end_and_unflushed_writes_are_lost() {
    disk_scenario::raw_flush_survives_boot_end_and_unflushed_writes_are_lost(&backend());
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and disk fixtures"]
fn tcg_disk_ext4_fsync_survives_crash_and_shutdown_sync_keeps_dirty_data() {
    disk_scenario::ext4_fsync_survives_crash_and_shutdown_sync_keeps_dirty_data(&backend());
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and disk fixtures"]
fn tcg_disk_snapshots_keep_both_layers_independently_without_promotion() {
    disk_scenario::snapshots_keep_both_layers_independently_without_promotion(&backend());
}
