#![cfg(all(seedbox_pv, target_arch = "x86_64"))]
//! Native instruction determinism through the public Driver boundary.
#[path = "support/epoch.rs"]
mod epoch;
use seedbox_machine::{
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    machine::Timestamp,
    pv::{Response, Stream},
    rng::RngStream,
};
use seedbox_machine_pv::machine::pv::Factory;
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{fs, path::PathBuf, time::Duration};
fn input(name: &str) -> PathBuf {
    std::env::var_os(name).expect(name).into()
}
/// Compile the freestanding x86 instruction fixture into an application archive.
#[cfg(target_arch = "x86_64")]
fn instruction_application(mode: &str) -> MachineDefinition {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("application");
    let status = seedbox_test::process::status(
        std::process::Command::new(input("SEEDBOX_PV_CLANG"))
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
                "/../seedbox-machine/tests/fixtures/pv-x86-instructions.c"
            ))
            .arg("-o")
            .arg(&output),
    )
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
        disk: None,
        network: None,
    }
}

/// Run the instruction fixture as an instrumented oracle through the public
/// Driver; return every committed output and the oracle's stdout.
#[cfg(target_arch = "x86_64")]
fn instruction_run(
    mode: &str,
    workers: usize,
    seed: u8,
) -> (Vec<seedbox_driver::driver::MachineResponse>, String) {
    use seedbox_driver::driver::{Driver, EventKind, StepOutcome};
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
    let mut factory = Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
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
        seedbox_driver::driver::NetworkConfig::default(),
        move |definition| factory.create(definition),
    )
    .unwrap();
    let guard = HangGuard::new(
        "PV instruction Driver worker transport",
        Duration::from_secs(300),
    );
    // pv-x86-instructions.c executes eight entropy operations or six child
    // probes. Allow 100,000 charged calls per operation for formatting and
    // fork/wait/signal settlement at the Driver default 1000 ns/call, plus
    // five simulated seconds for Linux boot. Neither mode has a sleep loop.
    let operations = match mode {
        "entropy" => 8,
        "probe" => 6,
        _ => panic!("unknown fixture mode {mode}"),
    };
    let deadline = Duration::from_secs(5) + Duration::from_nanos(operations * 100_000 * 1000);
    // One initial admission and one final oracle-outcome harvest may share a
    // barrier timestamp; all intervening work must advance committed time.
    let mut progress = GuestProgress::new(
        "instruction boot/finite operations/exit",
        Duration::ZERO,
        deadline,
        2,
    )
    .unwrap();
    let mut outputs = Vec::new();
    let mut last_result = None;
    loop {
        guard.remaining().unwrap();
        let now = driver
            .with_barrier(|b| Ok(b.playback().position.at))
            .unwrap();
        let upto = progress.next(Duration::from_nanos(now.as_nanos()), Duration::from_nanos(quantum))
            .unwrap_or_else(|error| panic!("{}: {error}; now={now:?}; last_result={last_result:?}; outputs={outputs:?}; pending=oracle exit", progress.phase()));
        let result = driver
            .step(Timestamp::from_nanos(upto.as_nanos().try_into().unwrap()))
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
        last_result = Some(result);
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
