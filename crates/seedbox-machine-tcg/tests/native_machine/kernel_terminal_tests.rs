use seedbox_machine::test_support::kernel_terminal_tests::*;
use std::{fs, path::PathBuf};
fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
#[test]
#[ignore = "requires native TCG QEMU, rebuilt assets and kernel-terminal fixture"]
fn tcg_guest_terminal_paths_exit_and_reboot() {
    let qemu = input("SEEDBOX_TCG_QEMU");
    let binary = fs::read(input("SEEDBOX_TCG_TERMINAL_APPLICATION")).unwrap();
    let mut factory = super::tcg::Factory::new(
        &qemu,
        &input("SEEDBOX_TCG_ASSETS"),
        super::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    for path in &PATHS {
        let machine = factory
            .create(&definition(&binary, path.mode, "terminal", 256 << 20))
            .unwrap();
        terminal_contract(path, machine, |saved| {
            super::tcg::TcgMachine::from_snapshot(&qemu, saved).unwrap()
        });
    }
}
