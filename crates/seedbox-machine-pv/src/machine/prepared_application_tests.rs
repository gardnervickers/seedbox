use crate::application::{boot_derived, with_runtime};
use seedbox_machine::test_support::prepared_fixture::bootstrap;
use seedbox_machine::test_support::prepared_image::*;
use std::fs;
#[cfg(seedbox_pv)]
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
            crate::machine::pv::PVMachine::from_boot_inputs(
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
