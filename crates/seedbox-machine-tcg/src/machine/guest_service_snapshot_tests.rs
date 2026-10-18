use seedbox_machine::test_support::guest_service_snapshot_tests::check;
use seedbox_machine::test_support::prepared_application as prepared;
use std::{fs, path::PathBuf};
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}

#[test]
#[ignore = "requires patched QEMU and static guest service initramfs"]
fn tcg_accepted_application_command_snapshot_continues_exactly_once() {
    use crate::machine::tcg::TcgMachine;
    let executable = input("SEEDBOX_TCG_QEMU");
    let bootstrap = seedbox_machine::test_support::prepared_fixture::bootstrap("TCG");
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        256 << 20,
    );
    let machine = TcgMachine::new(
        &executable,
        fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap(),
        &bootstrap,
        definition,
    )
    .unwrap();
    check(machine, |snapshot| {
        TcgMachine::from_snapshot(&executable, snapshot).unwrap()
    });
}
