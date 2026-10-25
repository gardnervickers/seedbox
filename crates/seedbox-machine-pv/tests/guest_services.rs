#![cfg(seedbox_pv)]
//! Native guest service acceptance through this backend's public Machine factory.
use seedbox_machine::test_support::guest_services::{applications, shutdown};
use seedbox_machine::test_support::prepared_application as prepared;
use seedbox_machine::test_support::TEST_EPOCH_SECONDS;
use std::{fs, path::PathBuf};
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}
#[cfg(seedbox_pv)]
fn pv() -> seedbox_machine_pv::machine::pv::PVMachine {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        128 << 20,
    );
    let mut backend = seedbox_machine_pv::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
        TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_SERVICE_INITRD static helper+fixture"]
fn pv_native_application_controls_logs_and_outcome() {
    applications(pv());
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_SERVICE_INITRD static helper+fixture"]
fn pv_native_shutdown_preserves_graceful_cleanup() {
    shutdown(pv());
}

#[test]
#[ignore = "requires native PV, full-state policy and SEEDBOX_PV_OOM_INITRD"]
fn pv_native_oom_and_sigkill_have_distinct_reports_and_fresh_evidence() {
    use seedbox_machine::{definition::BackendPolicy, test_support::guest_services::oom_outcome};
    for (mode, expect_oom) in [("oom", true), ("sigkill", false)] {
        let run = || {
            let mut definition =
                prepared::definition(fs::read(input("SEEDBOX_PV_OOM_INITRD")).unwrap(), 128 << 20);
            definition.application.launch.argv.push(mode.into());
            let mut factory = seedbox_machine_pv::machine::pv::Factory::new(
                &input("SEEDBOX_PV_WORKER"),
                &input("SEEDBOX_PV_ASSETS"),
                TEST_EPOCH_SECONDS,
            )
            .unwrap()
            .with_policy(BackendPolicy {
                full_state: true,
                ..Default::default()
            })
            .unwrap();
            oom_outcome(
                factory.create(&definition).unwrap(),
                expect_oom,
                |snapshot| {
                    seedbox_machine_pv::machine::pv::PVMachine::from_snapshot(
                        &input("SEEDBOX_PV_WORKER"),
                        snapshot,
                    )
                    .unwrap()
                },
            )
        };
        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "{mode}: independent fresh outputs, witness and full state"
        );
    }
}
