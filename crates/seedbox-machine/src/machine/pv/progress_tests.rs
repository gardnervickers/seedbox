//! Actual private DynamoRIO injection; wrapper-only tests are not this proof.
use super::*;
use crate::{
    application::{boot_derived, with_runtime, GuestBootstrap, Kernel},
    clock::{Clock, SimulationClock},
    definition::{FilesystemArchive, Launch, PreparedApplication},
    execution::ExecutionConfig,
    pv::{Inbox, Outbox, Output, Response, Stream},
    rng::SeededRng,
};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};
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
            command_line: "console=ttyAMA0 nosmp".into(),
        },
        init: fs::read(input("SEEDBOX_PV_INIT")).unwrap(),
        base_initramfs: Vec::new(),
    };
    let archive = if instrumented {
        let runtime =
            FilesystemArchive::new(fs::read(input("SEEDBOX_PV_PROGRESS_RUNTIME")).unwrap())
                .unwrap();
        let prefix = [
            "/seedbox/progress/bin64/drrun",
            "-opt_speed",
            "-disable_traces",
            "-ldstex2cas",
            "-c",
            "/seedbox/progress/libseedbox_progress.so",
            "--blocks-per-progress-call",
            "32",
            "--",
        ]
        .map(str::to_owned);
        boot_derived(&with_runtime(&app, &runtime, &prefix).unwrap(), &bootstrap).unwrap()
    } else {
        crate::application::boot_archive(&app, &bootstrap).unwrap()
    };
    PVMachine::from_boot_inputs(&input("SEEDBOX_PV_WORKER"), bootstrap.kernel.image, archive)
        .unwrap()
}
struct Session {
    machine: PVMachine,
    clock: SimulationClock,
    rng: SeededRng,
    events: Vec<Output>,
    started: Instant,
}
impl Session {
    fn new(machine: PVMachine) -> Self {
        Self {
            machine,
            clock: SimulationClock::new(0),
            rng: SeededRng::new(17),
            events: Vec::new(),
            started: Instant::now(),
        }
    }
    fn run(&mut self, budget: u64, factor: u32) {
        assert!(
            self.started.elapsed() < Duration::from_secs(120),
            "instrumentation fixture timed out: {:?}",
            self.events
        );
        let now = Timestamp::from_nanos(self.clock.now());
        let upto = Timestamp::from_nanos(self.clock.now() + budget);
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
    let raw_short = Session::new(machine(200_000, false)).finish(1_000_000, 1000);
    let raw_long = Session::new(machine(400_000, false)).finish(1_000_000, 1000);
    assert!(
        raw_short < 100_000 && raw_long < 100_000,
        "raw CPU work unexpectedly charged: {raw_short}/{raw_long}"
    );
    let short = Session::new(machine(200_000, true)).finish(1_000_000, 1000);
    let long = Session::new(machine(400_000, true)).finish(1_000_000, 1000);
    assert!(
        short > 1_000_000 && long > short * 3 / 2,
        "instrumented cost must grow with CPU iterations"
    );
    let doubled = Session::new(machine(400_000, true)).finish(1_000_000, 2000);
    assert!(
        doubled > long * 19 / 10 && doubled < long * 21 / 10,
        "measured CPU region rate: {long}/{doubled}"
    );
    let partitioned = Session::new(machine(400_000, true)).finish(137_000, 1000);
    assert_eq!(
        partitioned, long,
        "host run partition must not change charged CPU work"
    );
    let mut original = Session::new(machine(2_000_000, true));
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
        started: Instant::now(),
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
