use seedbox_machine::{application::GuestEnvironment, test_support::kernel_terminal_tests::*};
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
    let environment = GuestEnvironment::new("terminal", "127.0.0.1 localhost\n", false);
    for path in &PATHS {
        // Boot from a retained archive file, as the engine does. The contract
        // reboots this machine from the same file and a restored copy from
        // its snapshot's bytes, and requires the two boots to match.
        let definition = definition(&binary, path.mode, "terminal", 256 << 20);
        let archive = factory
            .boot_archive_configured(&definition, &environment)
            .unwrap();
        let retained = tempfile::NamedTempFile::new().unwrap();
        fs::write(retained.path(), &archive).unwrap();
        let machine = factory
            .create_boot_archive_file(&definition, retained.path(), archive.len() as u64)
            .unwrap();
        terminal_contract(path, machine, |saved| {
            super::tcg::TcgMachine::from_snapshot(&qemu, saved).unwrap()
        });
    }
}
