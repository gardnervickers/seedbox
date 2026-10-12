//! Explicit diagnostic guest for seedbox-executor; never selected by normal
//! app acceptance. The ELF and initramfs are built from that crate's tools.
#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
mod hvf {
    use seedbox_arm64::{
        application::Progress,
        backend::hvf::Hvf,
        machine::{Machine, MachineStop},
    };
    use serde_json::json;
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    #[test]
    #[ignore = "explicit experimental executor ELF and output paths required"]
    fn translated_spinloop_returns_at_real_pv_horizons() {
        let input = |key| std::fs::read(std::env::var_os(key).expect(key)).unwrap();
        let output =
            PathBuf::from(std::env::var_os("SEEDBOX_EXECUTOR_PROOF_OUTPUT").expect("output path"));
        let kernel = input("SEEDBOX_ARM64_KERNEL");
        let initrd = input("SEEDBOX_EXECUTOR_PROOF_INITRD");
        let mut machine = Machine::<Hvf>::new(&kernel, &initrd, 1)
            .unwrap()
            .with_progress(&Progress {
                speed_hz: 1_000_000_000,
                ns: 50_000,
                jitter_ns: 0,
                seed: 1,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut progress = Vec::new();
        let mut unmarked = Vec::new();
        let mut pending: Option<(bool, u64, usize)> = None;
        let mut lines = 0;
        let mut powered_off = false;
        while Instant::now() < deadline {
            let horizon = (machine.time_ns() / 10_000 + 1) * 10_000;
            let stop = machine.run_slice(horizon, 0, true).unwrap();
            if matches!(stop, MachineStop::Horizon { .. }) {
                if let Some((_, _, count)) = &mut pending {
                    *count += 1;
                }
            }
            let text = String::from_utf8_lossy(machine.console());
            for line in text
                .split_inclusive('\n')
                .filter(|l| l.ends_with('\n'))
                .skip(lines)
            {
                lines += 1;
                match line.trim() {
                    "EXECUTOR_PV_ENTER" | "EXECUTOR_UNMARKED_ENTER" => {
                        assert!(pending.is_none());
                        pending = Some((line.trim() == "EXECUTOR_PV_ENTER", machine.time_ns(), 0));
                    }
                    "EXECUTOR_PV_RETURN" | "EXECUTOR_UNMARKED_RETURN" => {
                        let (marked, start, horizons) = pending.take().expect("paired entry");
                        assert_eq!(marked, line.trim() == "EXECUTOR_PV_RETURN");
                        let delta = machine.time_ns() - start;
                        let evidence = json!({"start_ns":start,"end_ns":machine.time_ns(),"elapsed_ns":delta,"vmm_horizon_returns":horizons});
                        if marked {
                            progress.push(evidence);
                        } else {
                            unmarked.push(evidence);
                        }
                    }
                    _ => (),
                }
            }
            if stop == MachineStop::PoweredOff {
                powered_off = true;
                break;
            }
        }
        let console = String::from_utf8_lossy(machine.console());
        let evidence = json!({"architecture":"aarch64","backend":"hvf","progress_ns":50000,"horizon_ns":10000,"marked_calls":progress,"unmarked_calls":unmarked,"powered_off":powered_off,"console":console});
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(output)
            .unwrap();
        file.write_all(serde_json::to_string_pretty(&evidence).unwrap().as_bytes())
            .unwrap();
        assert!(powered_off, "guest wall timeout: {console}");
        assert!(
            console.lines().any(|l| l.trim() == "SEEDBOX_INIT_PASS"),
            "{console}"
        );
        assert!(!console.contains("EXECUTOR_FAIL") && !console.contains("EXECUTOR_PANIC"));
        assert_eq!(progress.len(), 3);
        for call in &progress {
            assert!(call["elapsed_ns"].as_u64().unwrap() >= 50_000, "{call}");
            assert!(call["vmm_horizon_returns"].as_u64().unwrap() >= 4, "{call}");
        }
        assert_eq!(unmarked.len(), 1);
        assert!(
            unmarked[0]["elapsed_ns"].as_u64().unwrap() < 20_000,
            "unmarked control: {}",
            unmarked[0]
        );
        assert!(pending.is_none());
    }
}
