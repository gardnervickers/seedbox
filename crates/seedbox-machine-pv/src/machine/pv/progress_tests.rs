//! Actual private DynamoRIO injection; wrapper-only tests are not this proof.
use super::*;
use crate::{
    clock::{Clock, SimulationClock},
    rng::SeededRng,
};
use seedbox_machine::application::{boot_derived, with_runtime, GuestBootstrap, Kernel};
use seedbox_machine::definition::{FilesystemArchive, Launch, PreparedApplication};
use seedbox_machine::execution::ExecutionConfig;
use seedbox_machine::pv::{Inbox, Outbox, Output, Response, Stream};
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
#[cfg(target_arch = "aarch64")]
const COMMAND_LINE: &str = "console=ttyAMA0 nosmp";
#[cfg(target_arch = "x86_64")]
const COMMAND_LINE: &str = "console=ttyS0";
fn input(name: &str) -> PathBuf {
    std::env::var_os(name).expect(name).into()
}
fn machine(iterations: u64, instrumented: bool) -> PVMachine {
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
    let app = PreparedApplication {
        filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
        launch: Launch {
            argv: vec!["/application".into(), iterations.to_string()],
            environment: BTreeMap::new(),
            working_directory: "/".into(),
            user: String::new(),
        },
    };
    let bootstrap = GuestBootstrap {
        kernel: Kernel {
            image: fs::read(input("SEEDBOX_PV_KERNEL")).unwrap(),
            command_line: COMMAND_LINE.into(),
        },
        init: fs::read(input("SEEDBOX_PV_INIT")).unwrap(),
        base_initramfs: Vec::new(),
        epoch_seconds: 0,
    };
    let archive = if instrumented {
        let runtime =
            FilesystemArchive::new(fs::read(input("SEEDBOX_PV_PROGRESS_RUNTIME")).unwrap())
                .unwrap();
        let prefix =
            super::factory::runtime_prefix(seedbox_spec::progress::ProgressBudget::MACHINE_DEFAULT);
        boot_derived(&with_runtime(&app, &runtime, &prefix).unwrap(), &bootstrap).unwrap()
    } else {
        seedbox_machine::application::boot_archive(&app, &bootstrap).unwrap()
    };
    PVMachine::from_boot_inputs(&input("SEEDBOX_PV_WORKER"), bootstrap.kernel.image, archive)
        .unwrap()
}
fn workload_deadline(iterations: u64, factor: u32) -> Duration {
    // cpu-loop.c executes one two-instruction block per iteration. Upper-bound
    // it by one PV call per iteration (the client actually batches blocks),
    // plus a million calls for loader/stdio/exit at the selected ns/call cost.
    // Five simulated seconds allow Linux boot/device timer settlement, not
    // application work; the fixture itself has no sleeps or input waits.
    Duration::from_secs(5) + Duration::from_nanos((iterations + 1_000_000) * u64::from(factor))
}
struct Session {
    machine: PVMachine,
    clock: SimulationClock,
    rng: SeededRng,
    events: Vec<Output>,
    progress: GuestProgress,
    guard: HangGuard,
}
impl Session {
    fn new(machine: PVMachine, iterations: u64, factor: u32) -> Self {
        let deadline = workload_deadline(iterations, factor);
        Self {
            machine,
            clock: SimulationClock::new(0),
            rng: SeededRng::new(17),
            events: Vec::new(),
            progress: GuestProgress::new(
                "CPU fixture boot/work/outcome",
                Duration::ZERO,
                deadline,
                0,
            )
            .unwrap(),
            guard: HangGuard::new(
                "native CPU fixture worker transport",
                Duration::from_secs(300),
            ),
        }
    }
    fn run(&mut self, budget: u64, factor: u32) {
        self.guard.remaining().unwrap();
        let limit = self
            .progress
            .next(
                Duration::from_nanos(self.clock.now()),
                Duration::from_nanos(budget),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{}: {error}; now={}; outputs={:?}",
                    self.progress.phase(),
                    self.clock.now(),
                    self.events
                )
            });
        let now = Timestamp::from_nanos(self.clock.now());
        let upto = Timestamp::from_nanos(limit.as_nanos().try_into().unwrap());
        let mut inbox = VecDeque::new();
        let result = self.machine.run(
            &mut RunContext {
                execution: ExecutionConfig::new(factor).unwrap(),
                worker: &mut (),
                clock: &self.clock,
                rng: &mut self.rng,
                inbox: Inbox::new(&mut inbox, now),
                outbox: Outbox::new(&mut self.events, now, upto),
            },
            upto,
        );
        assert!(result.is_ok(), "{result:?}; outputs={:?}", self.events);
        assert!(
            !matches!(result.unwrap(), StopReason::Exited { .. }),
            "application outcome must leave guest alive"
        );
        self.clock.set(upto.as_nanos());
    }
    fn stdout(&self) -> Vec<u8> {
        self.events
            .iter()
            .filter_map(|e| match &e.payload {
                Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes,
                } => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }
    fn finish(&mut self, budget: u64, factor: u32) -> u64 {
        while !self
            .events
            .iter()
            .any(|e| matches!(e.payload, Response::ApplicationOutcome { .. }))
        {
            self.run(budget, factor);
        }
        let stderr: Vec<_> = self
            .events
            .iter()
            .filter_map(|e| match &e.payload {
                Response::ApplicationOutput {
                    stream: Stream::Stderr,
                    bytes,
                } => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
        assert!(
            self.events
                .iter()
                .any(|e| matches!(e.payload, Response::ApplicationOutcome { code: 0 })),
            "{:?}",
            self.events
        );
        let text = String::from_utf8(self.stdout()).unwrap();
        let delta = text
            .split("DELTA=")
            .nth(1)
            .expect(&text)
            .trim()
            .parse()
            .unwrap();
        println!("factor={factor} budget={budget}: {text}");
        delta
    }
}
#[test]
#[ignore = "requires signed native PV worker, ABI3 kernel, isolated progress runtime and CPU fixture"]
fn native_instrumented_cpu_loop_has_work_cost_rate_and_snapshot_continuation() {
    let raw_short = Session::new(machine(200_000, false), 200_000, 1000).finish(1_000_000, 1000);
    let raw_long = Session::new(machine(400_000, false), 400_000, 1000).finish(1_000_000, 1000);
    assert!(
        raw_short < 100_000 && raw_long < 100_000,
        "raw CPU work unexpectedly charged: {raw_short}/{raw_long}"
    );
    let short = Session::new(machine(200_000, true), 200_000, 1000).finish(1_000_000, 1000);
    let long = Session::new(machine(400_000, true), 400_000, 1000).finish(1_000_000, 1000);
    assert!(
        short > 1_000_000 && long > short * 3 / 2,
        "instrumented cost must grow with CPU iterations"
    );
    let doubled = Session::new(machine(400_000, true), 400_000, 2000).finish(1_000_000, 2000);
    assert!(
        doubled > long * 19 / 10 && doubled < long * 21 / 10,
        "measured CPU region rate: {long}/{doubled}"
    );
    let partitioned = Session::new(machine(400_000, true), 400_000, 1000).finish(137_000, 1000);
    assert_eq!(
        partitioned, long,
        "host run partition must not change charged CPU work"
    );
    let mut original = Session::new(machine(2_000_000, true), 2_000_000, 1000);
    while !original.stdout().starts_with(b"START=") {
        original.run(1_000_000, 1000);
    }
    assert!(
        !original.stdout().windows(4).any(|w| w == b"END="),
        "snapshot must be inside CPU region"
    );
    let snapshot = original.machine.snapshot().unwrap();
    let now = original.clock.now();
    let rng = original.rng.clone();
    let prefix = original.events.clone();
    // Equivalent continuations keep the original absolute workload horizon.
    let progress = GuestProgress::new(
        "restored CPU continuation",
        Duration::from_nanos(now),
        workload_deadline(2_000_000, 1000),
        0,
    )
    .unwrap();
    let expected = original.finish(137_000, 1000);
    assert!(
        original.clock.now() - now > 10_000_000,
        "snapshot must retain substantial CPU-region work, not just buffered END output"
    );
    let outputs: Vec<_> = original
        .events
        .iter()
        .filter(|e| !matches!(e.payload, Response::BootConsole { .. }))
        .cloned()
        .collect();
    drop(original);
    let mut restored = Session {
        machine: PVMachine::from_snapshot(&input("SEEDBOX_PV_WORKER"), &snapshot).unwrap(),
        clock: SimulationClock::new(now),
        rng,
        events: prefix,
        progress,
        guard: HangGuard::new("restored CPU worker transport", Duration::from_secs(300)),
    };
    assert_eq!(restored.finish(137_000, 1000), expected);
    assert_eq!(
        restored
            .events
            .into_iter()
            .filter(|e| !matches!(e.payload, Response::BootConsole { .. }))
            .collect::<Vec<_>>(),
        outputs
    );
}

#[test]
fn cpu_workload_horizon_caps_partitions_and_restored_continuation() {
    let deadline = workload_deadline(2_000_000, 1000);
    assert_eq!(deadline, Duration::from_secs(8));
    assert_eq!(
        workload_deadline(400_000, 2000),
        Duration::from_millis(7800)
    );
    let snapshot = deadline - Duration::from_nanos(100);
    let mut original = GuestProgress::new("original", Duration::ZERO, deadline, 0).unwrap();
    let mut restored = GuestProgress::new("restored", snapshot, deadline, 0).unwrap();
    for progress in [&mut original, &mut restored] {
        assert_eq!(
            progress.next(snapshot, Duration::from_nanos(137_000)),
            Ok(deadline)
        );
        // A completed phase needs no next call at the edge. An unfinished
        // phase cannot renew its budget, nor retry at the same instant.
        assert_eq!(
            progress.next(snapshot, Duration::from_nanos(1)),
            Err(seedbox_test_time::ProgressError::NoProgress)
        );
        assert_eq!(
            progress.next(snapshot - Duration::from_nanos(1), Duration::from_nanos(1)),
            Err(seedbox_test_time::ProgressError::Backwards)
        );
        assert_eq!(
            progress.next(deadline, Duration::from_nanos(1)),
            Err(seedbox_test_time::ProgressError::Deadline)
        );
    }
}
