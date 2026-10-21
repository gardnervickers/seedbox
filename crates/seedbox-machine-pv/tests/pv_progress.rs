#![cfg(seedbox_pv)]
//! Public factory activation of real instrumentation, measured inside a CPU loop.
use seedbox_machine::test_support::TEST_EPOCH_SECONDS;
use seedbox_machine::{
    clock::Clock,
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    execution::ExecutionConfig,
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::{Inbox, Outbox, Response, Stream},
    rng::RngStream,
};
use seedbox_machine_pv::machine::pv::Factory;
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{collections::VecDeque, fs, path::PathBuf, time::Duration};
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
        disk: None,
        network: None,
    };
    let mut machine = factory.create(&definition).unwrap();
    let mut clock = TestClock(0);
    let mut rng = TestRng;
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    let guard = HangGuard::new("public PV CPU worker transport", Duration::from_secs(300));
    // cpu-loop.c has one two-instruction block per iteration. Bound each by
    // one PV call (the client actually batches), plus a million calls for
    // loader/stdio/exit at the selected ns/call CPU factor. Linux boot/device
    // settlement gets a separate five simulated-second allowance; no app sleeps.
    let deadline =
        Duration::from_secs(5) + Duration::from_nanos((iterations + 1_000_000) * u64::from(factor));
    let mut progress =
        GuestProgress::new("factory CPU boot/work/outcome", Duration::ZERO, deadline, 0).unwrap();
    let mut last_stop = None;
    loop {
        guard.remaining().unwrap();
        let limit = progress.next(Duration::from_nanos(clock.now()), Duration::from_millis(1))
            .unwrap_or_else(|error| panic!("{}: {error}; now={}; last_stop={last_stop:?}; outputs={outputs:?}; pending=application outcome", progress.phase(), clock.now()));
        let now = Timestamp::from_nanos(clock.now());
        let upto = Timestamp::from_nanos(limit.as_nanos().try_into().unwrap());
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
        last_stop = Some(stop);
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
    let mut factory = Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
        TEST_EPOCH_SECONDS,
    )
    .unwrap();
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
