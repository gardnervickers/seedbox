//! Actual vCPU entry, cancellation and architectural restore require the native
//! kernel API. Mock backend policy is covered by seedbox-arm64 integration tests.
use seedbox_arm64::backend::Backend;
fn busy_wait_elapsed(console: &str) -> Option<u64> {
    console
        .lines()
        .find_map(|line| line.trim().strip_prefix("busy-wait elapsed="))
        .and_then(|value| value.parse().ok())
        .filter(|ns| (200_000..=300_000).contains(ns))
}
mod guest {
    use super::busy_wait_elapsed;
    use seedbox_arm64::{backend::Backend, machine::Machine, witness::Report};
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
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
    fn suite<B: Backend>(name: &str, fast: bool) {
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
            "{name} {label}: {} exits={} wall={:.6}s virtual={}ns deferred_spis={} deferred_clock_read_spis={}",
            report.witness, report.exits, report.wall_seconds, report.virtual_ns,
            report.deferred_spi_events, report.deferred_clock_read_spis
        );
        };
        let a = Machine::<B>::new(&kernel, &initrd, 1)
            .unwrap()
            .run(|| {})
            .unwrap();
        save("seed1-a", &a);
        let c = Machine::<B>::new(&kernel, &initrd, 2)
            .unwrap()
            .run(|| {})
            .unwrap();
        save("seed2", &c);
        assert_ne!(a.witness, c.witness);
        // CPU load and randomly chosen host sleeps must not change any guest byte.
        let burner = Burner::start();
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
                std::thread::sleep(Duration::from_micros(rng % 5001));
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
        for report in [&a, &c, &d] {
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
        if fast {
            assert!(
            a.wall_seconds < 1.0,
            "baseline HVF boot including 200us guest timerfd spin and 1s sleep took {:.3}s; report timing failure (1s budget)",
            a.wall_seconds
        );
        }
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(super) fn hvf_determinism() {
        suite::<seedbox_arm64::backend::hvf::Hvf>("hvf", true);
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    pub(super) fn kvm_determinism() {
        suite::<seedbox_arm64::backend::kvm::Kvm>("kvm", false);
    }
}

// This readback checks the native register API; mocks cover the policy's bit math.
fn cpu_features(backend: &mut impl Backend) {
    use seedbox_arm64::backend::SysReg;
    let mmfr = backend.get_sys_reg(SysReg::ID_AA64MMFR0).unwrap();
    let dfr = backend.get_sys_reg(SysReg::ID_AA64DFR0).unwrap();
    seedbox_arm64::machine::configure_cpu_features(backend).unwrap();
    assert_eq!(
        backend.get_sys_reg(SysReg::ID_AA64MMFR0).unwrap(),
        mmfr & !(0xf << 56)
    );
    assert_eq!(
        backend.get_sys_reg(SysReg::ID_AA64DFR0).unwrap(),
        dfr & !(0xf << 8)
    );
    assert_eq!(backend.get_sys_reg(SysReg::MPIDR).unwrap(), 0x8000_0000);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn hvf_checkpoint_receipt_restores_in_a_new_worker_generation() {
    use seedbox_arm64::{application::worker::*, backend::hvf::Hvf};
    use std::{fs, os::unix::net::UnixStream, time::Duration};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
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
            .set_read_timeout(Some(Duration::from_secs(10)))
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn hvf_stopped_state() {
    use seedbox_arm64::backend::{hvf::Hvf, CoreReg};
    let mut backend = Hvf::create().unwrap();
    backend.create_vcpu().unwrap();
    cpu_features(&mut backend);
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

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn kvm_cpu_features() {
    let mut backend = seedbox_arm64::backend::kvm::Kvm::create().unwrap();
    backend.create_vcpu().unwrap();
    cpu_features(&mut backend);
}

#[test]
#[ignore = "requires the native ARM hypervisor"]
fn native() {
    let _kernel = super::support::kernel();
    #[cfg(target_os = "macos")]
    {
        hvf_stopped_state();
        canceled_entry_holds_no_instruction_and_preserves_pc();
        hvf_checkpoint_receipt_restores_in_a_new_worker_generation();
    }
    #[cfg(target_os = "linux")]
    kvm_cpu_features();
}

#[test]
#[ignore = "requires native ARM hypervisor and fixture guest inputs"]
fn guests() {
    let _kernel = super::support::kernel();
    #[cfg(target_os = "macos")]
    guest::hvf_determinism();
    #[cfg(target_os = "linux")]
    guest::kvm_determinism();
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn canceled_entry_holds_no_instruction_and_preserves_pc() {
    use seedbox_arm64::backend::{hvf::Hvf, Backend, CoreReg, Exit, RunStop};
    let mut backend = Hvf::create().unwrap();
    let memory =
        std::rc::Rc::new(seedbox_arm64::memory::Memory::new(seedbox_arm64::PAGE_SIZE).unwrap());
    // HVC #0 is a guaranteed finite natural exit once the queued cancel
    // has returned. It must still execute at the original PC.
    unsafe {
        memory.write(0, &0xd400_0002u32.to_le_bytes()).unwrap();
    }
    backend
        .map_memory(0, seedbox_arm64::RAM_BASE, memory)
        .unwrap();
    backend.create_vcpu().unwrap();
    backend.set_reg(CoreReg::Pstate, 0x3c5).unwrap();
    backend
        .set_reg(CoreReg::Pc, seedbox_arm64::RAM_BASE)
        .unwrap();
    for _ in 0..3 {
        backend.request_exit().unwrap();
        assert!(matches!(backend.run().unwrap(), RunStop::Interrupted));
        assert_eq!(
            backend.get_reg(CoreReg::Pc).unwrap(),
            seedbox_arm64::RAM_BASE
        );
        assert_eq!(
            backend.complete(None).unwrap_err(),
            "no HVF exit to complete"
        );
    }
    let RunStop::Guest(stop) = backend.run().unwrap() else {
        panic!("stale cancellation survived its observed stop");
    };
    assert_eq!(stop.pc, seedbox_arm64::RAM_BASE);
    assert!(matches!(stop.exit, Exit::Hvc { imm: 0 }));
    assert_eq!(
        backend.get_reg(CoreReg::Pc).unwrap(),
        seedbox_arm64::RAM_BASE + 4
    );
    assert!(backend
        .run()
        .unwrap_err()
        .contains("before completing held HVF exit"));
    backend.complete(None).unwrap();
    assert_eq!(
        backend.get_reg(CoreReg::Pc).unwrap(),
        seedbox_arm64::RAM_BASE + 4
    );
}
