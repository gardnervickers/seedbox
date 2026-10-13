//! Private runtime derivation/wrapper interactions; public construction lives in integration tests.
use crate as machine_crate;
#[path = "../../tests/support/prepared_image.rs"]
mod image;
use super::prepared_fixture::bootstrap;
use crate::application::{boot_derived, with_runtime};
use image::*;
use std::fs;
#[test]
#[ignore = "requires native PV worker/kernel and rebuilt prepared fixtures"]
fn pv_native_prepared_image_launch_and_runtime_wrapper() {
    let worker = input("SEEDBOX_PV_WORKER");
    let bootstrap = bootstrap("PV");
    let binary = fs::read(input("SEEDBOX_PV_PREPARED_FIXTURE")).unwrap();
    for script in [false, true] {
        let definition = definition(&binary, script);
        let derived = with_runtime(
            &definition.application,
            &archive(&binary, true),
            &[
                "/seedbox/progress/wrapper".into(),
                "--wrapper".into(),
                "--".into(),
            ],
        )
        .unwrap();
        check(
            super::pv::PVMachine::from_boot_inputs(
                &worker,
                bootstrap.kernel.image.clone(),
                boot_derived(&derived, &bootstrap).unwrap(),
            )
            .unwrap(),
            script,
            true,
        );
    }
}
#[test]
#[ignore = "requires native TCG QEMU/kernel and rebuilt prepared fixtures"]
fn tcg_native_prepared_image_launch_and_runtime_wrapper() {
    use super::tcg::{Boot, Manifest, TcgMachine};
    let qemu = input("SEEDBOX_TCG_QEMU");
    let bootstrap = bootstrap("TCG");
    let firmware = fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap();
    let binary = fs::read(input("SEEDBOX_TCG_PREPARED_FIXTURE")).unwrap();
    for script in [false, true] {
        let definition = definition(&binary, script);
        let derived = with_runtime(
            &definition.application,
            &archive(&binary, true),
            &[
                "/seedbox/progress/wrapper".into(),
                "--wrapper".into(),
                "--".into(),
            ],
        )
        .unwrap();
        check(
            TcgMachine::new_raw(
                &qemu,
                Manifest {
                    memory_mib: 128,
                    boot: Boot::Linux {
                        kernel: bootstrap.kernel.image.clone(),
                        initramfs: boot_derived(&derived, &bootstrap).unwrap(),
                        firmware: firmware.clone(),
                        command_line: bootstrap.kernel.command_line.clone(),
                    },
                },
            )
            .unwrap(),
            script,
            true,
        );
    }
}
