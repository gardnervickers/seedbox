#![cfg(seedbox_pv)]
//! Public factory activation of real instrumentation, measured inside a CPU loop.
use seedbox_machine::{
    clock::Clock,
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    execution::ExecutionConfig,
    machine::{pv::Factory, Machine, RunContext, StopReason, Timestamp},
    pv::{Inbox, Outbox, Response, Stream},
    rng::RngStream,
};
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
struct TestClock(u64);
impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0
    }
}
struct TestRng;
impl RngStream for TestRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        bytes.fill(0);
    }
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name).expect(name).into()
}
fn measured(factory: &mut Factory, iterations: u64, factor: u32) -> u64 {
    // The existing static fixture samples CLOCK_MONOTONIC around a native
    // countdown loop, with no syscall or polling inside the measured region.
    let binary = fs::read(input("SEEDBOX_PV_PROGRESS_FIXTURE")).unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(binary.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "application", binary.as_slice())
        .unwrap();
    let definition = MachineDefinition {
        name: "cpu".into(),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
            launch: Launch {
                argv: vec!["/application".into(), iterations.to_string()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
    };
    let mut machine = factory.create(&definition).unwrap();
    let mut clock = TestClock(0);
    let mut rng = TestRng;
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "CPU fixture timed out"
        );
        let now = Timestamp::from_nanos(clock.now());
        let upto = Timestamp::from_nanos(clock.now() + 1_000_000);
        let stop = machine
            .run(
                &mut RunContext {
                    execution: ExecutionConfig::new(factor).unwrap(),
                    worker: &mut (),
                    clock: &clock,
                    rng: &mut rng,
                    inbox: Inbox::new(&mut inbox, now),
                    outbox: Outbox::new(&mut outputs, now, upto),
                },
                upto,
            )
            .unwrap();
        assert!(!matches!(stop, StopReason::Exited { .. }), "{stop:?}");
        clock.0 = upto.as_nanos();
        if let Some(code) = outputs.iter().find_map(|o| match o.payload {
            Response::ApplicationOutcome { code } => Some(code),
            _ => None,
        }) {
            assert_eq!(code, 0);
            break;
        }
    }
    let stream = |selected| {
        outputs
            .iter()
            .filter_map(|o| match &o.payload {
                Response::ApplicationOutput { stream, bytes } if *stream == selected => {
                    Some(bytes.as_slice())
                }
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>()
    };
    let stderr = stream(Stream::Stderr);
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
    let stdout = String::from_utf8(stream(Stream::Stdout)).unwrap();
    let delta = stdout
        .split("DELTA=")
        .nth(1)
        .expect(&stdout)
        .trim()
        .parse()
        .unwrap();
    println!("iterations={iterations} factor={factor}: {stdout}");
    delta
}
#[test]
#[ignore = "requires native PV worker, factory guest assets and existing CPU-loop fixture"]
fn factory_automatically_charges_cpu_work_and_execution_rate() {
    let mut factory =
        Factory::new(&input("SEEDBOX_PV_WORKER"), &input("SEEDBOX_PV_ASSETS")).unwrap();
    let short = measured(&mut factory, 200_000, 1000);
    let long = measured(&mut factory, 400_000, 1000);
    assert!(
        short > 1_000_000 && long > short * 3 / 2,
        "CPU work must advance time: {short}/{long}"
    );
    let slow = measured(&mut factory, 400_000, 2000);
    assert!(
        slow > long * 19 / 10 && slow < long * 21 / 10,
        "CPU rate must scale measured work: {long}/{slow}"
    );
}

/// Compile the freestanding x86 instruction fixture into an application archive.
#[cfg(target_arch = "x86_64")]
fn instruction_application(mode: &str) -> MachineDefinition {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("application");
    let status = std::process::Command::new(input("SEEDBOX_PV_CLANG"))
        .args([
            "--target=x86_64-linux-gnu",
            "-mstackrealign",
            "-nostdlib",
            "-static",
            "-fno-stack-protector",
            "-O2",
            "-Wl,-e,_start",
        ])
        .arg(format!("-fuse-ld={}", input("SEEDBOX_PV_LD").display()))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/pv-x86-instructions.c"
        ))
        .arg("-o")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());
    let binary = fs::read(output).unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(binary.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "application", binary.as_slice())
        .unwrap();
    MachineDefinition {
        name: "oracle".into(),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
            launch: Launch {
                argv: vec!["/application".into(), mode.into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
    }
}

/// Run the instruction fixture as an instrumented oracle through the public
/// Driver; return every committed output and the oracle's stdout.
#[cfg(target_arch = "x86_64")]
fn instruction_run(
    mode: &str,
    workers: usize,
    seed: u8,
) -> (Vec<seedbox_machine::driver::MachineResponse>, String) {
    use seedbox_machine::driver::{Driver, EventKind, StepOutcome};
    use seedbox_machine::machine::ExitReason;
    struct SeedRng(u8);
    impl RngStream for SeedRng {
        fn fill_bytes(&mut self, bytes: &mut [u8]) {
            for byte in bytes {
                *byte = self.0;
                self.0 = self.0.wrapping_add(1);
            }
        }
    }
    let mut factory =
        Factory::new(&input("SEEDBOX_PV_WORKER"), &input("SEEDBOX_PV_ASSETS")).unwrap();
    let quantum = 5_000_000;
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        Duration::from_nanos(quantum),
        workers,
        SeedRng(seed),
        || Ok(()),
        instruction_application(mode),
        vec![],
        vec![],
        move |definition| factory.create(definition),
    )
    .unwrap();
    let started = Instant::now();
    let mut outputs = Vec::new();
    loop {
        assert!(started.elapsed() < Duration::from_secs(180), "{outputs:?}");
        let now = driver
            .with_barrier(|b| Ok(b.playback().position.at))
            .unwrap();
        let result = driver
            .step(Timestamp::from_nanos(now.as_nanos() + quantum))
            .unwrap();
        outputs.extend(
            driver
                .drain_events()
                .into_iter()
                .filter_map(|event| match event.kind {
                    EventKind::Output(output) => Some(output),
                    _ => None,
                }),
        );
        if let StepOutcome::Finished { reason, .. } = result {
            assert_eq!(reason, ExitReason::ProgramExit { code: 0 }, "{outputs:?}");
            break;
        }
    }
    let stdout = outputs
        .iter()
        .filter_map(|o| match &o.payload {
            Response::ApplicationOutput {
                stream: Stream::Stdout,
                bytes,
            } => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    (outputs, String::from_utf8(stdout).unwrap())
}

#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires native x86 PV worker, factory guest assets and SEEDBOX_PV_CLANG/SEEDBOX_PV_LD"]
fn instrumented_rdrand_and_rdseed_draw_seeded_guest_entropy() {
    let (serial, stdout) = instruction_run("entropy", 1, 9);
    println!("{stdout}");
    let (parallel, parallel_stdout) = instruction_run("entropy", 4, 9);
    assert_eq!(stdout, parallel_stdout);
    assert!(serial == parallel, "worker count changed committed outputs");
    let (_, reseeded) = instruction_run("entropy", 1, 77);
    let parse = |text: &str| -> Vec<(String, u64, u64)> {
        text.lines()
            .map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                assert_eq!(fields.len(), 3, "{line}");
                let number = |field: &str| u64::from_str_radix(field, 16).unwrap();
                (fields[0].to_owned(), number(fields[1]), number(fields[2]))
            })
            .collect()
    };
    let draws = parse(&stdout);
    let names: Vec<_> = draws.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "rdrand16", "rdrand32", "rdrand64", "rdrand64", "rdseed16", "rdseed32", "rdseed64",
            "rdseed64"
        ]
    );
    for (name, value, flags) in &draws {
        // Success: CF set, OF/SF/ZF/AF/PF cleared from their preset values.
        assert_eq!(*flags, 1, "{name}");
        // 16-bit writes keep the sentinel above; 32-bit writes zero-extend.
        if name.ends_with("16") {
            assert_eq!(value >> 16, 0xa5a5_a5a5_a5a5, "{name}");
        } else if name.ends_with("32") {
            assert_eq!(value >> 32, 0, "{name}");
        }
    }
    let wide: Vec<u64> = draws
        .iter()
        .filter(|d| d.0.ends_with("64"))
        .map(|d| d.1)
        .collect();
    let mut unique = wide.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), wide.len(), "repeated draws: {stdout}");
    let other: Vec<u64> = parse(&reseeded)
        .iter()
        .filter(|d| d.0.ends_with("64"))
        .map(|d| d.1)
        .collect();
    assert!(
        wide.iter().zip(&other).all(|(a, b)| a != b),
        "seeds must select different entropy: {stdout} / {reseeded}"
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "requires native x86 PV worker, factory guest assets and SEEDBOX_PV_CLANG/SEEDBOX_PV_LD"]
fn host_sensitive_user_instructions_are_virtual_or_refused() {
    let (serial, stdout) = instruction_run("probe", 1, 9);
    println!("{stdout}");
    let (parallel, parallel_stdout) = instruction_run("probe", 4, 9);
    assert_eq!(stdout, parallel_stdout);
    assert!(serial == parallel, "worker count changed committed outputs");
    assert_eq!(stdout, instruction_run("probe", 1, 77).1);
    // RDPID reads the guest-owned TSC_AUX (CPU 0). RDPRU and UMWAIT are
    // refused (SIGILL); RDPMC faults without CR4.PCE (SIGSEGV). RDTSCP traps
    // to the kernel's virtual time. KVM intercepts MONITORX/MWAITX, so the
    // timed wait completes without host-time dependence.
    assert_eq!(
        stdout.replace(' ', ""),
        "rdpid0000000000000000\n\
         rdprusignal0000000000000004\n\
         rdpmcsignal000000000000000b\n\
         rdtscp00000000000000010000000000000000\n\
         monitorx0000000000000001\n\
         umwaitsignal0000000000000004\n"
    );
}
