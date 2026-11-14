//! Real Linux on native PV with one attached disk: the shared scenarios of
//! `machine/disk_scenario.rs`.
use crate::machine::pv::{Factory, PVMachine, Restored, Retention, Snapshot};
use seedbox_machine::test_support::disk_scenario::{self, Backend};
use std::{cell::RefCell, path::PathBuf};

fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
fn factory() -> Factory {
    Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap()
}
fn restore(snapshot: &Snapshot) -> Result<PVMachine, std::io::Error> {
    PVMachine::from_snapshot(&input("SEEDBOX_PV_WORKER"), snapshot).map_err(std::io::Error::other)
}

fn backend() -> Backend<PVMachine> {
    Backend {
        factory: || {
            let mut factory = factory();
            Box::new(move |definition| factory.create(definition))
        },
        restore,
        store: PVMachine::disk_store,
        probe: input("SEEDBOX_PV_DISK_PROBE"),
        image: input("SEEDBOX_PV_DISK_IMAGE"),
        memory_bytes: 128 << 20,
    }
}

thread_local! {
    /// How [`retaining`] produced each machine it restored on this thread.
    static RESTORED: RefCell<Vec<Restored>> = const { RefCell::new(Vec::new()) };
}
/// As [`backend`], but its machines keep the snapshots of their live boots
/// in their workers, checking each one and each rewind to it.
fn retaining() -> Backend<PVMachine> {
    Backend {
        factory: || {
            let mut factory = factory().with_retained_snapshots(Retention {
                limit_bytes: u64::MAX,
                verify: true,
            });
            Box::new(move |definition| factory.create(definition))
        },
        restore: |snapshot| {
            let machine = restore(snapshot)?;
            RESTORED.with_borrow_mut(|restored| restored.extend(machine.restored()));
            Ok(machine)
        },
        ..backend()
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

/// The same scenario with its live snapshot retained. Every restore of it
/// comes after the boot of the machine last in its worker has ended, so where
/// the backend retains, each goes back to it in that worker.
#[test]
#[ignore = "requires native PV worker, rebuilt assets and disk fixtures"]
fn pv_disk_retained_snapshots_keep_both_layers_independently_without_promotion() {
    disk_scenario::snapshots_keep_both_layers_independently_without_promotion(&retaining());
    let restored = RESTORED.take();
    if super::RETAINS {
        // The copy that FLUSHes, the copy that loses the write, the ended
        // source, and the copy that source is compared with.
        assert!(
            matches!(
                restored[..],
                [
                    Restored::Rewound { .. },
                    Restored::Rewound { .. },
                    Restored::Deferred,
                    Restored::Rewound { .. }
                ]
            ),
            "{restored:?}"
        );
    } else {
        let loaded = Restored::Loaded;
        assert_eq!(restored, [loaded, loaded, Restored::Deferred, loaded]);
    }
}
