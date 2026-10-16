//! Real Linux on native PV with one attached disk: the shared scenarios of
//! `machine/disk_scenario.rs`.
use crate::machine::disk_scenario::Backend;
use seedbox_machine_pv::machine::pv::{Factory, PVMachine};
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
        store: |_| None,
        probe: input("SEEDBOX_PV_DISK_PROBE"),
        image: input("SEEDBOX_PV_DISK_IMAGE"),
        memory_bytes: 128 << 20,
    }
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and disk fixtures"]
fn pv_disk_driver_is_deterministic_across_workers() {
    super::disk_driver_scenario::driver_is_deterministic_across_workers(&backend());
}
