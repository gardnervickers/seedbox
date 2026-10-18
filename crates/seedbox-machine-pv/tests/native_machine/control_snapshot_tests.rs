use seedbox_machine::test_support::{
    prepared_application as prepared, simulation_control as control,
};
fn prepared_machine() -> seedbox_machine_pv::machine::pv::PVMachine {
    let mut factory = seedbox_machine_pv::machine::pv::Factory::new(
        &control::input("SEEDBOX_PV_WORKER"),
        &control::input("SEEDBOX_PV_ASSETS"),
        super::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    factory
        .create(&prepared::definition(
            std::fs::read(control::input("SEEDBOX_PV_CONTROL_INITRD")).unwrap(),
            128 << 20,
        ))
        .unwrap()
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_CONTROL_INITRD"]
fn pv_native_control_pending_reply_snapshot_delivers_once() {
    control::pending_snapshot(prepared_machine(), |snapshot| {
        seedbox_machine_pv::machine::pv::PVMachine::from_snapshot(
            &control::input("SEEDBOX_PV_WORKER"),
            snapshot,
        )
        .unwrap()
    });
}
