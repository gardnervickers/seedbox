//! Native tests are selected by Cargo features and run by harness/check.sh.
//! Tests without hardware features never open a hypervisor.
#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
use seedbox_arm64::backend::Backend;
fn busy_wait_elapsed(console: &str) -> Option<u64> {
    console
        .lines()
        .find_map(|line| line.trim().strip_prefix("busy-wait elapsed="))
        .and_then(|value| value.parse().ok())
        .filter(|ns| (200_000..=300_000).contains(ns))
}
#[test]
fn busy_wait_requires_hrtimer_duration() {
    assert_eq!(
        busy_wait_elapsed("busy-wait elapsed=200017\r\n"),
        Some(200017)
    );
    for console in [
        "busy-wait elapsed=199999",
        "busy-wait elapsed=300001",
        "busy-wait elapsed=10000000",
        "busy-wait elapsed=garbage",
        "SEEDBOX_INIT_PASS",
    ] {
        assert_eq!(busy_wait_elapsed(console), None);
    }
}

mod jitter {
    use std::{sync::mpsc, time::Duration};

    /// Vary host scheduling by completing real work on a separate thread.
    /// Each guest exit waits for its own completion signal, never a host delay.
    pub(super) struct Jitter {
        requests: Option<mpsc::Sender<u64>>,
        completed: mpsc::Receiver<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Jitter {
        pub(super) fn start() -> Self {
            let (requests, received) = mpsc::channel::<u64>();
            let (done, completed) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                for iterations in received {
                    let mut work = iterations;
                    for _ in 0..iterations {
                        work = work.wrapping_mul(6364136223846793005).wrapping_add(1);
                        std::hint::black_box(work);
                    }
                    if done.send(()).is_err() {
                        break;
                    }
                }
            });
            Self {
                requests: Some(requests),
                completed,
                thread: Some(thread),
            }
        }
        pub(super) fn perturb(&self, iterations: u64) {
            self.requests.as_ref().unwrap().send(iterations).unwrap();
            self.completed
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    }
    impl Drop for Jitter {
        fn drop(&mut self) {
            self.requests.take();
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn host_jitter_waits_for_each_work_completion() {
        let jitter = Jitter::start();
        for iterations in [0, 1, 5000, 0] {
            jitter.perturb(iterations);
        }
    }
}

#[cfg(all(
    feature = "guest-tests",
    any(
        all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"),
        all(target_os = "linux", target_arch = "aarch64", feature = "kvm-tests")
    )
))]
mod guest {
    use super::{busy_wait_elapsed, jitter::Jitter};
    use seedbox_arm64::{backend::Backend, machine::Machine, witness::Report};
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    fn same(a: &Report, b: &Report) {
        if a.witness != b.witness {
            let first = a.trace.iter().zip(&b.trace).position(|(x, y)| x != y);
            panic!("witness mismatch\nfirst differing exit: {first:?}\nleft: {:?}\nright: {:?}\nexit lengths: {} / {}\nconsole hashes: {} / {}\nRAM hashes: {} / {}", first.and_then(|i| a.trace.get(i)), first.and_then(|i| b.trace.get(i)), a.exits, b.exits, a.console_hash, b.console_hash, a.ram_hash, b.ram_hash);
        }
    }
    struct Burner {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Burner {
        fn start() -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let signal = stop.clone();
            let thread = std::thread::spawn(move || {
                let mut n = 1u64;
                while !signal.load(Ordering::Relaxed) {
                    n = n.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(n);
                }
            });
            Self {
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for Burner {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }
    fn suite<B: Backend>(name: &str) {
        let input = |key| {
            let path = std::env::var_os(key)
                .unwrap_or_else(|| panic!("set {key}; see docs/internals.md#development-checks"));
            std::fs::read(&path).unwrap()
        };
        let kernel = input("SEEDBOX_ARM64_KERNEL");
        let initrd = input("SEEDBOX_ARM64_INITRD");
        let output = PathBuf::from(
            std::env::var_os("SEEDBOX_ARM64_REPORT_DIR")
                .expect("set SEEDBOX_ARM64_REPORT_DIR to a fresh output directory"),
        );
        std::fs::create_dir_all(&output).unwrap();
        let save = |label: &str, report: &Report| {
            use std::io::Write;
            let path = output.join(format!("{name}-{label}.json"));
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            f.write_all(&serde_json::to_vec_pretty(report).unwrap())
                .unwrap();
            eprintln!(
            "{name} {label}: {} exits={} virtual={}ns deferred_spis={} deferred_clock_read_spis={}",
            report.witness, report.exits, report.virtual_ns,
            report.deferred_spi_events, report.deferred_clock_read_spis
        );
        };
        let a = Machine::<B>::new(&kernel, &initrd, 1)
            .unwrap()
            .run(|| {})
            .unwrap();
        save("seed1-a", &a);
        let b = Machine::<B>::new(&kernel, &initrd, 1)
            .unwrap()
            .run(|| {})
            .unwrap();
        save("seed1-b", &b);
        same(&a, &b);
        let c = Machine::<B>::new(&kernel, &initrd, 2)
            .unwrap()
            .run(|| {})
            .unwrap();
        save("seed2", &c);
        assert_ne!(a.witness, c.witness);
        // CPU load and randomly sized work on another thread must not change any guest byte.
        let burner = Burner::start();
        let jitter = Jitter::start();
        use std::io::Read;
        let mut entropy = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut entropy)
            .unwrap();
        let mut rng = u64::from_ne_bytes(entropy) | 1;
        let d = Machine::<B>::new(&kernel, &initrd, 1)
            .unwrap()
            .run(|| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                jitter.perturb(rng % 5001);
            })
            .unwrap();
        drop(burner);
        save("seed1-jitter", &d);
        same(&a, &d);
        assert!(
            a.console.contains("sleep requested=1000000000 elapsed=10"),
            "guest must verify a 1s sleep: {}",
            a.console
        );
        for report in [&a, &b, &c, &d] {
            assert!(
            busy_wait_elapsed(&report.console).is_some(),
            "guest must complete the 200us timerfd spin within 300us; rebuild kernel-arm64/init.c: {}",
            report.console
        );
            // init only passes after observing exactly one timerfd expiration
            // while spinning. Deferral counts are diagnostic metadata; the mock
            // review suites prove masked-then-unmasked delivery independently.
        }
        assert!(
            a.trace
                .iter()
                .any(|r| r.kind == 1 && r.write && r.addr == seedbox_arm64::EVENT_ADDR + 24),
            "busy-wait must cross a polling bound"
        );
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
    #[test]
    fn hvf_determinism() {
        suite::<seedbox_arm64::backend::hvf::Hvf>("hvf");
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", feature = "kvm-tests"))]
    #[test]
    fn kvm_determinism() {
        suite::<seedbox_arm64::backend::kvm::Kvm>("kvm");
    }
}

// Read-only inventory around the shared feature policy. Keep host model drift
// visible when investigating matching exit traces with differing guest RAM.
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"),
    all(target_os = "linux", target_arch = "aarch64", feature = "kvm-tests")
))]
fn cpu_features<B: seedbox_arm64::backend::Backend>() {
    use seedbox_arm64::{backend::SysReg, machine::configure_cpu_features};
    let mut backend = B::create().unwrap();
    backend.create_vcpu().unwrap();
    for phase in ["host-default", "configured"] {
        if phase == "configured" {
            configure_cpu_features(&mut backend).unwrap();
        }
        for reg in [
            0xc000, 0xc005, 0xc006, // MIDR, MPIDR, REVIDR
            0xc020, 0xc021, // ID_AA64PFR0/1
            0xc028, 0xc029, // ID_AA64DFR0/1
            0xc030, 0xc031, 0xc032, // ID_AA64ISAR0/1/2
            0xc038, 0xc039, 0xc03a, // ID_AA64MMFR0/1/2
            0xc800, 0xd801, 0xd807, 0xdf00, // CLIDR, CTR, DCZID, CNTFRQ
        ] {
            match backend.get_sys_reg(SysReg(reg)) {
                Ok(value) => eprintln!("{phase} {reg:#06x}: {value:#018x}"),
                Err(error) => eprintln!("{phase} {reg:#06x}: unavailable: {error}"),
            }
        }
        for (name, offset) in [("GICD_TYPER", 4), ("GICD_IIDR", 8), ("GICD_PIDR2", 0xffe8)] {
            match backend.get_gic_dist(offset) {
                Ok(value) => eprintln!("{phase} {name}: {value:#018x}"),
                Err(error) => eprintln!("{phase} {name}: unavailable: {error}"),
            }
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
fn hvf_cpu_features() {
    cpu_features::<seedbox_arm64::backend::hvf::Hvf>();
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
fn checkpoint_guest() -> Vec<u8> {
    // A small Image-format guest handshakes with PV, increments X2 and yields through the PV idle register.
    // Every horizon therefore resumes real instructions after a completed MMIO exit.
    let mut kernel = vec![0u8; 92];
    kernel[..4].copy_from_slice(&0x1400_0010u32.to_le_bytes()); // B +64
    kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
    kernel[56..60].copy_from_slice(b"ARM\x64");
    let instructions = [
        0xd2a1_4021u32, // MOV X1, #0x0a010000 (PV event device)
        0xd280_0000 | ((seedbox_arm64::ABI as u32) << 5), // MOV X0, #ABI
        0xf900_1020,    // STR X0, [X1, #32] (ready)
        0x9100_0442,    // ADD X2, X2, #1
        0xf900_0820,    // STR X0, [X1, #16] (PV idle)
        0x17ff_fffe,    // B back to ADD
    ];
    for (offset, word) in instructions.iter().enumerate() {
        kernel[64 + offset * 4..68 + offset * 4].copy_from_slice(&word.to_le_bytes());
    }
    kernel
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
fn hvf_checkpoint_continues_an_entered_guest() {
    use seedbox_arm64::{backend::hvf::Hvf, boot::BootSpec, machine::Machine};
    let kernel = checkpoint_guest();
    let fresh = || {
        Machine::<Hvf>::for_application(
            BootSpec::for_application(&kernel, b"initrd", 1, 16 << 20, "").unwrap(),
            [0; 6],
            None,
        )
        .unwrap()
    };
    let mut original = fresh();
    original.run_until(10, || {}).unwrap();
    assert!(original.stopped_observation().exits > 0);
    let mut bytes = Vec::new();
    original
        .snapshot()
        .unwrap()
        .write_checkpoint(&mut bytes)
        .unwrap();
    original.run_until(20, || {}).unwrap();
    let expected = original.application_evidence(true).unwrap();
    let expected_cpu = serde_json::to_value(original.snapshot().unwrap()).unwrap()["cpu"].clone();
    drop(original);
    let mut restored = fresh().restore_checkpoint(bytes.as_slice()).unwrap();
    restored.run_until(20, || {}).unwrap();
    let actual_cpu = serde_json::to_value(restored.snapshot().unwrap()).unwrap()["cpu"].clone();
    for (group, values) in expected_cpu.as_object().unwrap() {
        match values {
            serde_json::Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    assert_eq!(&actual_cpu[group][index], value, "CPU {group}[{index}]");
                }
            }
            serde_json::Value::Object(values) => {
                for (reg, value) in values {
                    assert_eq!(&actual_cpu[group][reg], value, "CPU {group}[{reg}]");
                }
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(restored.application_evidence(true).unwrap(), expected);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
fn hvf_checkpoint_receipt_restores_in_a_new_worker_generation() {
    use seedbox_arm64::{application::worker::*, backend::hvf::Hvf};
    use std::{fs, os::unix::net::UnixStream, time::Duration};
    let root_owner = tempfile::tempdir().unwrap();
    let root = root_owner.path().join("checkpoint");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("kernel"), checkpoint_guest()).unwrap();
    fs::write(root.join("initrd"), b"initrd").unwrap();
    let boot = Boot {
        kernel: root.join("kernel"),
        initrd: root.join("initrd"),
        seed: 1,
        memory_bytes: 16 << 20,
        command_line: String::new(),
        mac: [0; 6],
        disk_image: None,
        disk_sectors: Some(8),
        disk_fault_model: seedbox_arm64::disk::DiskFaultModel {
            sector_bytes: 512,
            survival_probability: 1.0,
        },
        timer_policy: None,
    };
    let mut saved = None;
    let mut expected = None;
    for generation in [1, 2] {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let thread = std::thread::spawn(move || {
            let mut output = server.try_clone().unwrap();
            serve::<Hvf>(&mut server, &mut output)
        });
        let identity = Identity {
            name: "machine".into(),
            generation,
            incarnation: 0,
        };
        let mut sequence = 0;
        let mut call = |operation| {
            write_message(
                &mut client,
                &Request {
                    sequence,
                    expected: identity.clone(),
                    operation,
                },
            )
            .unwrap();
            let response = read_message::<Response>(&mut client).unwrap().unwrap();
            assert_eq!(response.sequence, sequence);
            assert_eq!(response.after.as_ref(), Some(&identity));
            assert!(!matches!(response.answer, Answer::Error(_)), "{response:?}");
            sequence += 1;
            response
        };
        call(Operation::Boot(boot.clone()));
        if generation == 1 {
            call(Operation::Advance {
                hard_limit: 10,
                early_slack: 0,
                observe_console: false,
            });
            let response = call(Operation::Snapshot {
                artifact: root.join("checkpoint"),
            });
            let Answer::Snapshot(checkpoint) = response.answer else {
                panic!("missing checkpoint")
            };
            assert_eq!(response.observation.as_ref(), Some(&checkpoint.observation));
            saved = Some(checkpoint);
        } else {
            let checkpoint = saved.clone().unwrap();
            let response = call(Operation::Restore(checkpoint.clone()));
            assert!(matches!(response.answer, Answer::Restored(ack) if ack == checkpoint));
            assert_eq!(response.observation.as_ref(), Some(&checkpoint.observation));
        }
        call(Operation::Advance {
            hard_limit: 20,
            early_slack: 0,
            observe_console: false,
        });
        let Answer::Evidence(evidence) = call(Operation::Evidence {
            full_state: true,
            ram_pages: None,
        })
        .answer
        else {
            panic!("missing evidence")
        };
        if generation == 1 {
            expected = Some(evidence);
        } else {
            assert_eq!(Some(evidence), expected);
        }
        call(Operation::Quit);
        drop(client);
        thread.join().unwrap().unwrap();
    }
    eprintln!("retained worker checkpoint: {}", root.display());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
#[test]
fn hvf_stopped_state() {
    use seedbox_arm64::backend::{hvf::Hvf, CoreReg};
    let mut backend = Hvf::create().unwrap();
    backend.create_vcpu().unwrap();
    seedbox_arm64::machine::configure_cpu_features(&mut backend).unwrap();
    backend.set_reg(CoreReg::X(3), 0x12345678).unwrap();
    let first = backend.state().unwrap();
    let second = backend.state().unwrap();
    assert_eq!(first, second, "stopped state must not contain host time");
    assert_eq!(first.core[3], 0x12345678);
    assert_eq!(first.simd.len(), 32);
    assert!(!first.gic.is_empty());
    backend.set_reg(CoreReg::X(3), 9).unwrap();
    assert_ne!(first, backend.state().unwrap());
    drop(backend);

    // HVF's GIC restore contract requires a new VM, before its first entry.
    let mut restored = Hvf::create().unwrap();
    restored.create_vcpu().unwrap();
    seedbox_arm64::machine::configure_cpu_features(&mut restored).unwrap();
    let mut snapshot = first;
    for (index, register) in snapshot.simd.iter_mut().enumerate() {
        for (byte, value) in register.iter_mut().enumerate() {
            *value = (index * 16 + byte) as u8;
        }
    }
    let mut invalid = snapshot.clone();
    invalid.core.pop();
    assert!(restored.restore_state(&invalid).is_err());
    restored.restore_state(&snapshot).unwrap();
    assert_eq!(restored.state().unwrap(), snapshot);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64", feature = "kvm-tests"))]
#[test]
fn kvm_cpu_features() {
    cpu_features::<seedbox_arm64::backend::kvm::Kvm>();
}
