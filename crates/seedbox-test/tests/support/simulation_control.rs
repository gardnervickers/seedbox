use super::prepared;
pub use seedbox_machine::test_support::simulation_control::*;
use std::fs;
mod epoch {
    include!("epoch.rs");
}
pub fn tcg() -> seedbox_machine_tcg::machine::tcg::TcgMachine {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_TCG_CONTROL_INITRD")).unwrap(),
        256 << 20,
    );
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}
#[cfg(seedbox_pv)]
pub fn pv() -> seedbox_machine_pv::machine::pv::PVMachine {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_CONTROL_INITRD")).unwrap(),
        128 << 20,
    );
    let mut backend = seedbox_machine_pv::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}
