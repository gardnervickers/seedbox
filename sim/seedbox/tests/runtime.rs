//! Explicit hardware acceptance through the public library. Ignored by portable
//! tests; run accept-runtime.sh on a qualified Linux KVM host.
#![cfg(target_os = "linux")]
use seedbox::{
    config::{self, Simulation},
    execution::*,
    fixture::Frozen,
    oci::OciTools,
    orchestration::*,
    preparation::OciPreparer,
    runner,
    store::ContentStore,
    templates::{self, Template},
    vmm::VmmExecutor,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn env(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .into()
}
fn registers(cpu: u32) -> BTreeMap<u32, u64> {
    runner::cpu_list(
        &fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        ))
        .unwrap(),
    )
    .unwrap()
    .into_iter()
    .map(|cpu| {
        let mut data = [0; 8];
        fs::File::open(format!("/dev/cpu/{cpu}/msr"))
            .unwrap()
            .read_exact_at(&mut data, 0xc0011020)
            .unwrap();
        (cpu, u64::from_le_bytes(data))
    })
    .collect()
}
struct Restore {
    cpu: u32,
    original: BTreeMap<u32, u64>,
}
impl Restore {
    fn verify(&self) {
        assert_eq!(registers(self.cpu), self.original, "exact MSR restoration");
    }
}
impl Drop for Restore {
    fn drop(&mut self) {
        self.verify();
    }
}
fn same(run: &Run, replay: &Run) {
    assert_eq!(replay.record.outcome, run.record.outcome);
    assert_eq!(
        replay.record.replay.as_ref().unwrap().status,
        ReplayStatus::Match
    );
    assert_ne!(run.directory, replay.directory);
}
fn prepare(sim: &Simulation, backend: &VmmExecutor, store: &ContentStore) -> Frozen {
    let preparer = OciPreparer {
        tools: OciTools {
            docker: "podman".into(),
            ..OciTools::default()
        },
        kernel: env("SEEDBOX_TEST_KERNEL"),
        engine: env("SEEDBOX_TEST_VMM"),
        init: env("SEEDBOX_TEST_INIT"),
        busybox: env("SEEDBOX_BUSYBOX"),
        profile: backend.profile(),
    };
    Frozen {
        simulation: sim.clone(),
        inputs: preparer.prepare(sim, store).unwrap(),
    }
}
fn execute(
    orchestrator: &Orchestrator,
    frozen: &Frozen,
    backend: &VmmExecutor,
    guard: &Restore,
) -> Run {
    let run = orchestrator
        .run(
            &frozen.simulation,
            RunOptions {
                seed: Some(1009),
                ..RunOptions::default()
            },
            frozen,
            backend,
        )
        .unwrap();
    guard.verify();
    println!("{:?}: {}", run.record.outcome, run.directory.display());
    run
}
fn replay(orchestrator: &Orchestrator, run: &Run, backend: &VmmExecutor, guard: &Restore) -> Run {
    let again = orchestrator
        .replay(&run.directory, backend, &CancellationToken::default())
        .unwrap();
    guard.verify();
    same(run, &again);
    again
}
#[test]
#[ignore = "requires approved Linux KVM/MSR access and OCI tools; use accept-runtime.sh"]
fn workloads_controls_and_lifecycle_use_the_library() {
    let cpu = std::env::var("SEEDBOX_TEST_CPU").unwrap().parse().unwrap();
    let output = env("SEEDBOX_TEST_OUTPUT");
    let guard = Restore {
        cpu,
        original: registers(cpu),
    };
    fs::write(
        output.join("msr-before.json"),
        serde_json::to_vec_pretty(&guard.original).unwrap(),
    )
    .unwrap();
    let state = output.join("state");
    let store = ContentStore::new(state.join("objects"));
    let orchestrator = Orchestrator::new(store.clone(), state.join("runs"));
    let backend = VmmExecutor::new(
        &env("SEEDBOX_TEST_VMM"),
        &store,
        cpu,
        1024,
        WitnessMode::FullState,
    )
    .unwrap();
    let mut redis = None;
    for (name, template) in [("redis", Template::Redis), ("etcd", Template::Etcd)] {
        templates::init(&output.join(name), template).unwrap();
        let sim = config::load(&output.join(name).join("compose.yaml"), &BTreeMap::new()).unwrap();
        let frozen = prepare(&sim, &backend, &store);
        let run = execute(&orchestrator, &frozen, &backend, &guard);
        assert_eq!(run.record.outcome, Outcome::Pass, "{:?}", run.record);
        let again = replay(&orchestrator, &run, &backend, &guard);
        assert_eq!(again.record.replay.unwrap().full_state_matches, Some(true));
        let mut negative = sim.clone();
        let client = negative.services.get_mut("client").unwrap();
        if name == "redis" {
            client
                .environment
                .insert("SEEDBOX_PLANT_FAILURE".into(), Some("1".into()));
        } else {
            client
                .command
                .as_mut()
                .unwrap()
                .extend(["--plant".into(), "missing".into()]);
        }
        let failed = execute(
            &orchestrator,
            &prepare(&negative, &backend, &store),
            &backend,
            &guard,
        );
        assert_eq!(failed.record.outcome, Outcome::Fail, "{:?}", failed.record);
        replay(&orchestrator, &failed, &backend, &guard);
        if name == "redis" {
            redis = Some((frozen, run));
        }
    }
    let (frozen, baseline) = redis.unwrap();
    for (margin, undo) in [("10000", 256), ("adaptive", 0), ("adaptive", 16)] {
        let varied = VmmExecutor::from_profile(backend.profile(), cpu)
            .unwrap()
            .with_margin(margin)
            .unwrap()
            .with_undo(undo)
            .unwrap();
        replay(&orchestrator, &baseline, &varied, &guard);
    }
    let mut short = frozen.clone();
    short.simulation.limits.simulated_time_ns = 1_000_000;
    let timed = execute(&orchestrator, &short, &backend, &guard);
    assert_eq!(timed.record.outcome, Outcome::Timeout);
    replay(&orchestrator, &timed, &backend, &guard);
    let mut wall = frozen.clone();
    wall.simulation.limits.wall_time_ns = 1_000_000;
    let timed = execute(&orchestrator, &wall, &backend, &guard);
    assert_eq!(timed.record.outcome, Outcome::Incomplete);
    assert_eq!(
        timed.record.report.unwrap().reason,
        StopReason::WallTimeLimit
    );
    let timed = orchestrator
        .replay(&timed.directory, &backend, &CancellationToken::default())
        .unwrap();
    guard.verify();
    assert_eq!(
        timed.record.replay.unwrap().status,
        ReplayStatus::NotComparable
    );

    // Keep one CLI boundary check: signal the owned CLI only after its actual
    // VMM child exists. All other workload assertions above use the Rust API.
    let log = output.join("signal.log");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args([
            "replay",
            baseline.directory.to_str().unwrap(),
            "--cpu",
            &cpu.to_string(),
            "--state",
            state.to_str().unwrap(),
        ])
        .stdout(fs::File::create(&log).unwrap())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let children =
            fs::read_to_string(format!("/proc/{}/task/{}/children", child.id(), child.id()))
                .unwrap_or_default();
        if !children.trim().is_empty() {
            break;
        }
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI did not start a VMM before cancellation");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert_eq!(child.wait().unwrap().code(), Some(2));
    guard.verify();
    let text = fs::read_to_string(log).unwrap();
    let path = text
        .lines()
        .find_map(|l| l.strip_prefix("INCOMPLETE: "))
        .expect("CLI result path");
    let cancelled = inspect(Path::new(path)).unwrap();
    assert_eq!(cancelled.report.unwrap().reason, StopReason::Cancelled);
    assert_eq!(
        cancelled.replay.unwrap().status,
        ReplayStatus::NotComparable
    );
    fs::write(
        output.join("msr-after.json"),
        serde_json::to_vec_pretty(&registers(cpu)).unwrap(),
    )
    .unwrap();
    println!("PASS: library workloads, planted failures, full-state/performance replay, timeouts, CLI cancellation and exact MSR restoration");
}
