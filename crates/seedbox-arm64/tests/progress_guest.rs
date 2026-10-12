//! Dedicated fixture gate; the generic guest initrd is not a translated spin loop.
//! Compile with the normal portable HVF feature check. Execute explicitly with
//! the retained kernel and pv-spin-init.c initrd after portable checks pass.
#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
#[ignore = "requires SEEDBOX_ARM64_PROGRESS_INITRD containing the release progress runtime"]
fn owned_progress_reaches_exact_real_kernel_horizons() {
    use seedbox_arm64::{
        application::Progress,
        backend::hvf::Hvf,
        boot::{BootSpec, DEFAULT_COMMAND_LINE},
        machine::{Machine, MachineStop},
    };
    use std::{fs, path::PathBuf, time::Instant};

    let input = |key| fs::read(std::env::var_os(key).expect(key)).unwrap();
    let kernel = input("SEEDBOX_ARM64_KERNEL");
    let initrd = input("SEEDBOX_ARM64_PROGRESS_INITRD");
    let output = PathBuf::from(std::env::var_os("SEEDBOX_ARM64_REPORT_DIR").unwrap());
    fs::create_dir_all(&output).unwrap();
    let started = Instant::now();
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
        let mut ready_at = None;
        for horizon in (1..=2_000).map(|n| n * 1_000_000) {
            assert!(
                started.elapsed().as_secs() < 120,
                "PV fixture wall deadline"
            );
            assert_eq!(
                machine.run_until(horizon, || {}).unwrap(),
                MachineStop::Horizon { time_ns: horizon }
            );
            assert_eq!(machine.stopped_observation().time_ns, horizon);
            if String::from_utf8_lossy(machine.console()).contains("PV-PROGRESS-READY") {
                ready_at = Some(horizon);
                break;
            }
        }
        let ready_at = ready_at.expect("translated application never reached its spin loop");
        // Uneven increments cross actual 50us payment boundaries and retain any
        // owed charge. An equal horizon must not execute or alter full state.
        let mut stops = Vec::new();
        for delta in [1, 39, 49_999, 50_000, 50_001, 100_013, 200_027] {
            let horizon = ready_at + delta;
            assert_eq!(
                machine.run_until(horizon, || {}).unwrap(),
                MachineStop::Horizon { time_ns: horizon }
            );
            assert_eq!(machine.stopped_observation().time_ns, horizon);
            let before = machine.application_evidence(true).unwrap();
            assert_eq!(
                machine.run_until(horizon, || {}).unwrap(),
                MachineStop::Horizon { time_ns: horizon }
            );
            assert_eq!(machine.application_evidence(true).unwrap(), before);
            stops.push(before);
        }
        let console = String::from_utf8_lossy(machine.console()).into_owned();
        for failure in [
            "Internal Error",
            "Segmentation fault",
            "Kernel panic",
            "fatal",
        ] {
            assert!(!console.contains(failure), "{console}");
        }
        fs::write(
            output.join(format!("progress-{repetition}-console.txt")),
            console,
        )
        .unwrap();
        fs::write(
            output.join(format!("progress-{repetition}-horizons.json")),
            serde_json::to_vec_pretty(&stops).unwrap(),
        )
        .unwrap();
        eprintln!("real PV progress: run={repetition} ready_at={ready_at} exact_horizons=7 full_state=true");
        if let Some(expected) = &baseline {
            assert_eq!(&stops, expected);
        } else {
            baseline = Some(stops);
        }
    }
}
