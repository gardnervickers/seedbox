//! Real Linux on the TCG reference machine with one attached disk: the
//! shared scenarios of `machine/disk_scenario.rs`, as on native PV.
use crate::machine::disk_scenario::Backend;
use seedbox_machine_tcg::machine::tcg::{Factory, TcgMachine};
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
        store: |_| None,
        probe: input("SEEDBOX_TCG_DISK_PROBE"),
        image: input("SEEDBOX_TCG_DISK_IMAGE"),
        memory_bytes: 128 << 20,
    }
}

#[test]
#[ignore = "requires patched QEMU, TCG assets and disk fixtures"]
fn tcg_disk_driver_is_deterministic_across_workers() {
    super::disk_driver_scenario::driver_is_deterministic_across_workers(&backend());
}
