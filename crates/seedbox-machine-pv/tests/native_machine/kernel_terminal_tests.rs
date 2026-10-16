use seedbox_machine::test_support::kernel_terminal_tests::*;
use std::{fs, path::PathBuf};
fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV worker, rebuilt assets and kernel-terminal fixture"]
fn pv_guest_terminal_paths_exit_and_reboot() {
    let worker = input("SEEDBOX_PV_WORKER");
    let binary = fs::read(input("SEEDBOX_PV_TERMINAL_APPLICATION")).unwrap();
    let mut factory = seedbox_machine_pv::machine::pv::Factory::new(
        &worker,
        &input("SEEDBOX_PV_ASSETS"),
        super::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    for path in PATHS.iter().chain(std::iter::once(&PV_HALT)) {
        let machine = factory
            .create(&definition(&binary, path.mode, "terminal", 128 << 20))
            .unwrap();
        terminal_contract(path, machine, |saved| {
            seedbox_machine_pv::machine::pv::PVMachine::from_snapshot(&worker, saved).unwrap()
        });
    }
}
