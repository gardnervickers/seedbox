use crate::application::{boot_derived, with_runtime};
use seedbox_machine::test_support::prepared_fixture::bootstrap;
use seedbox_machine::test_support::prepared_image::*;
use std::fs;
#[test]
#[ignore = "requires native TCG QEMU/kernel and rebuilt prepared fixtures"]
fn tcg_native_prepared_image_launch_and_runtime_wrapper() {
    use crate::machine::tcg::{Boot, Manifest, TcgMachine};
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
                    disk: None,
                    network: None,
                    boot: Boot::Linux {
                        kernel: bootstrap.kernel.image.clone().into(),
                        initramfs: boot_derived(&derived, &bootstrap).unwrap().into(),
                        firmware: firmware.clone().into(),
                        command_line: bootstrap.kernel.command_line.clone(),
                        epoch_seconds: 0,
                    },
                },
            )
            .unwrap(),
            script,
            true,
        );
    }
}
