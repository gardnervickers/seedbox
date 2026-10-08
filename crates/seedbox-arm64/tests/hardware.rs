//! Native tests are selected by Cargo features and run by harness/check.sh.
//! Tests without hardware features never open a hypervisor.
#[cfg(any(feature = "hvf-tests", feature = "kvm-tests"))]
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

#[cfg(feature = "guest-tests")]
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
                .unwrap_or_else(|| panic!("set {key}; see docs/arm64-backends.md"));
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
        if fast {
            assert!(
            a.wall_seconds < 1.0,
            "baseline HVF boot including 200us guest timerfd spin and 1s sleep took {:.3}s; report timing failure (1s budget)",
            a.wall_seconds
        );
        }
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "hvf-tests"))]
    #[test]
    fn hvf_determinism() {
        suite::<seedbox_arm64::backend::hvf::Hvf>("hvf", true);
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", feature = "kvm-tests"))]
    #[test]
    fn kvm_determinism() {
        suite::<seedbox_arm64::backend::kvm::Kvm>("kvm", false);
    }
}

// Read-only inventory around the shared feature policy. Keep host model drift
// visible when investigating matching exit traces with differing guest RAM.
#[cfg(any(feature = "hvf-tests", feature = "kvm-tests"))]
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
}

#[cfg(all(target_os = "linux", target_arch = "aarch64", feature = "kvm-tests"))]
#[test]
fn kvm_cpu_features() {
    cpu_features::<seedbox_arm64::backend::kvm::Kvm>();
}
