use seedbox_machine::test_support::guest_service_snapshot_tests::check;
use seedbox_machine::test_support::prepared_application as prepared;
use std::{fs, path::PathBuf};
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}

#[test]
#[ignore = "requires a native PV worker with snapshots, kernel and static guest service initramfs"]
fn pv_accepted_application_command_snapshot_continues_exactly_once() {
    use crate::machine::pv::PVMachine;
    let executable = input("SEEDBOX_PV_WORKER");
    let bootstrap = seedbox_machine::test_support::prepared_fixture::bootstrap("PV");
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        128 << 20,
    );
    let machine = PVMachine::new(&executable, &bootstrap, definition).unwrap();
    check(machine, |snapshot| {
        PVMachine::from_snapshot(&executable, snapshot).unwrap()
    });
}
