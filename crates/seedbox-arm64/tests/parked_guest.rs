//! Dedicated real-PV publication fixtures; run explicitly after portable checks.
#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
fn run_case(mode: u32) {
    use seedbox_arm64::{
        application::Progress,
        backend::hvf::Hvf,
        boot::{BootSpec, DEFAULT_COMMAND_LINE},
        machine::{Machine, MachineStop},
    };
    use std::{fs, path::PathBuf, time::Instant};
    let kernel = fs::read(std::env::var_os("SEEDBOX_ARM64_KERNEL").unwrap()).unwrap();
    let inputs = PathBuf::from(std::env::var_os("SEEDBOX_ARM64_PARKED_INITRDS").unwrap());
    let output = PathBuf::from(std::env::var_os("SEEDBOX_ARM64_REPORT_DIR").unwrap());
    fs::create_dir_all(&output).unwrap();
    {
        let initrd = fs::read(inputs.join(format!("parked-{mode}.cpio"))).unwrap();
        let mut baseline = None;
        for repetition in 0..2 {
            let mut machine = Machine::<Hvf>::for_application(
                BootSpec::for_application(&kernel, &initrd, 7, 256 << 20, DEFAULT_COMMAND_LINE)
                    .unwrap(),
                [2, 0, 0, 0, 0, 1],
                None,
            )
            .unwrap()
            .with_progress(&Progress {
                speed_hz: 1_000_000_000,
                ns: 50_000,
                jitter_ns: 0,
                seed: 11,
            })
            .unwrap();
            let started = Instant::now();
            let mut completed = false;
            let mut horizons = 0;
            for horizon in (1..=20_000).map(|n| n * 1_000_000) {
                assert!(
                    started.elapsed().as_secs() < 120,
                    "parked PV fixture deadline"
                );
                let stop = machine.run_until(horizon, || {}).unwrap_or_else(|error| {
                    fs::write(
                        output.join(format!("parked-{mode}-{repetition}.console")),
                        machine.console(),
                    )
                    .unwrap();
                    panic!(
                        "parked guest: {error}; console retained in {}",
                        output.display()
                    );
                });
                if stop == MachineStop::PoweredOff {
                    completed = true;
                    break;
                }
                assert_eq!(stop, MachineStop::Horizon { time_ns: horizon });
                assert_eq!(machine.stopped_observation().time_ns, horizon);
                horizons += 1;
            }
            let console = String::from_utf8_lossy(machine.console()).into_owned();
            fs::write(
                output.join(format!("parked-{mode}-{repetition}.console")),
                &console,
            )
            .unwrap();
            assert!(completed, "fixture did not power off\n{console}");
            assert!(
                console.contains(&format!("PV-PARK-END mode={mode} success=1 status=0")),
                "{console}"
            );
            for failure in [
                "Internal Error",
                "Segmentation fault",
                "Kernel panic",
                "fatal",
            ] {
                assert!(!console.contains(failure), "{console}");
            }
            let evidence = machine.application_evidence(true).unwrap();
            fs::write(
                output.join(format!("parked-{mode}-{repetition}.json")),
                serde_json::to_vec_pretty(&evidence).unwrap(),
            )
            .unwrap();
            if let Some(expected) = &baseline {
                assert_eq!(&evidence, expected);
            } else {
                baseline = Some(evidence);
            }
            eprintln!("real PV parked: mode={mode} repetition={repetition} horizons={horizons} full_state=true");
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
#[ignore = "requires SEEDBOX_ARM64_PARKED_INITRDS containing the dedicated initrd"]
fn predicate_wait_preserves_real_kernel_context() {
    run_case(1);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
#[ignore = "requires SEEDBOX_ARM64_PARKED_INITRDS containing the dedicated initrd"]
fn owned_progress_preserves_real_kernel_context() {
    run_case(2);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
#[ignore = "requires SEEDBOX_ARM64_PARKED_INITRDS containing the dedicated initrd"]
fn nested_return_preserves_real_kernel_context() {
    run_case(3);
}
