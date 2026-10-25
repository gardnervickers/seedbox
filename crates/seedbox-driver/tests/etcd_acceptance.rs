#![cfg(unix)]
//! Real three-node etcd under the public Driver on one selected backend.
//!
//! Three etcd machines (10.0.1.1..3) each mount their own ext4 disk at `/data`
//! through the `etcd-node` wrapper and run the static etcd binary; the oracle
//! (10.0.1.10) is `etcd-client`, which talks to etcd's v3 JSON gateway over the
//! simulated network. The run ends when the oracle exits, which finishes the
//! Driver, or at the simulated limit. The host wall-time bound only diagnoses a
//! hang; it never shapes simulation results.
//!
//! Explicit and ignored: `cargo test -p seedbox-driver --test etcd_acceptance
//! -- --ignored`. Inputs:
//! - `SEEDBOX_ETCD_BACKEND`: `tcg` or `pv`, with that backend's usual inputs
//!   (`SEEDBOX_TCG_QEMU`/`SEEDBOX_TCG_ASSETS` or
//!   `SEEDBOX_PV_WORKER`/`SEEDBOX_PV_ASSETS`);
//! - `SEEDBOX_ETCD`: static etcd for the guest architecture;
//! - `SEEDBOX_ETCD_IMAGE`: the empty ext4 image every node's disk starts from;
//! - `SEEDBOX_ETCD_FIXTURES`: `etcd-test-fixtures.nix` output (`bin/etcd-node`,
//!   `bin/etcd-client`) for the guest architecture.
//!
//! Both backends use the same x86-64 etcd executable and complete machine
//! definitions. No architecture opt-in or application-specific runtime override
//! is supplied.
use seedbox_driver::driver::{Driver, Event, EventKind, MachineId, NetworkConfig, StepOutcome};
use seedbox_machine::test_support::TEST_EPOCH_SECONDS;
use seedbox_machine::{
    definition::{
        DiskImage, FilesystemArchive, Launch, MacAddress, MachineDefinition, NetworkInterface,
        PreparedApplication, Resources,
    },
    machine::{Machine, Timestamp},
    pv::{Response, Stream},
    rng::RngStream,
};
use seedbox_test_time::HangGuard;
use std::{collections::BTreeMap, fmt::Debug, fs, net::Ipv4Addr, path::PathBuf, time::Duration};

/// Simulated bound for one run.
const LIMIT: u64 = 180_000_000_000;
/// Host diagnostic only; it cannot interrupt a call that never returns.
const HOST_LIMIT: Duration = Duration::from_secs(30 * 60);
const QUANTUM: Duration = Duration::from_millis(10);
/// Every machine's memory; the size both backends support.
const MEMORY: u64 = 128 << 20;
/// Machine names in identity order: the oracle, then the etcd nodes.
const NAMES: [&str; 4] = ["client", "etcd-1", "etcd-2", "etcd-3"];

fn input(key: &str) -> PathBuf {
    std::env::var_os(key)
        .unwrap_or_else(|| panic!("set {key}"))
        .into()
}

/// The machine at `index` of [`NAMES`]: MAC 02:00:00:00:01:0N, 10.0.1.N.
fn interface(index: usize) -> NetworkInterface {
    let host = if index == 0 { 10 } else { index as u8 };
    NetworkInterface::new(
        MacAddress::new([2, 0, 0, 0, 1, host]).unwrap(),
        Ipv4Addr::new(10, 0, 1, host),
        24,
    )
    .unwrap()
}

fn peer(node: usize) -> String {
    format!("http://10.0.1.{node}:2380")
}

/// A root filesystem of executable files.
fn archive(files: &[(&str, &[u8])]) -> FilesystemArchive {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(&mut header, path, *bytes).unwrap();
    }
    FilesystemArchive::new(tar.into_inner().unwrap()).unwrap()
}

fn definition(
    index: usize,
    filesystem: FilesystemArchive,
    argv: Vec<String>,
    environment: BTreeMap<String, String>,
    disk: Option<DiskImage>,
) -> MachineDefinition {
    MachineDefinition {
        name: NAMES[index].into(),
        resources: Resources {
            memory_bytes: MEMORY,
        },
        application: PreparedApplication {
            filesystem,
            launch: Launch {
                argv,
                environment,
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk,
        network: Some(interface(index)),
    }
}

/// The oracle running `etcd-client STAGE`, then the three etcd nodes.
fn definitions(stage: &str) -> (MachineDefinition, Vec<MachineDefinition>) {
    let fixtures = input("SEEDBOX_ETCD_FIXTURES");
    let client = fs::read(fixtures.join("bin/etcd-client")).unwrap();
    let wrapper = fs::read(fixtures.join("bin/etcd-node")).unwrap();
    let etcd = fs::read(input("SEEDBOX_ETCD")).unwrap();
    // One immutable base; every node's disk starts from it independently.
    let image = DiskImage::from_bytes(fs::read(input("SEEDBOX_ETCD_IMAGE")).unwrap()).unwrap();
    let oracle = definition(
        0,
        archive(&[("application", &client)]),
        vec!["/application".into(), stage.into()],
        BTreeMap::new(),
        None,
    );
    let cluster = (1..=3)
        .map(|node| format!("etcd-{node}={}", peer(node)))
        .collect::<Vec<_>>()
        .join(",");
    let nodes = (1..=3)
        .map(|node| {
            let client_url = format!("http://10.0.1.{node}:2379");
            let argv = [
                "/application",
                "/dev/vda",
                "/data",
                "/etcd",
                "--name",
                NAMES[node],
                "--data-dir",
                "/data/etcd",
                "--listen-peer-urls",
                &peer(node),
                "--initial-advertise-peer-urls",
                &peer(node),
                "--listen-client-urls",
                &client_url,
                "--advertise-client-urls",
                &client_url,
                "--initial-cluster",
                &cluster,
                "--initial-cluster-token",
                "seedbox-etcd-acceptance",
                "--initial-cluster-state",
                "new",
            ]
            .map(String::from)
            .to_vec();
            definition(
                node,
                archive(&[("application", &wrapper), ("etcd", &etcd)]),
                argv,
                BTreeMap::new(),
                Some(image.clone()),
            )
        })
        .collect();
    (oracle, nodes)
}

/// The Driver's seeded RNG: one continuous splitmix64 byte stream, the same
/// however callers split their fills.
struct Seed {
    state: u64,
    word: [u8; 8],
    used: usize,
}
impl Seed {
    fn new(state: u64) -> Self {
        Self {
            state,
            word: [0; 8],
            used: 8,
        }
    }
}
impl RngStream for Seed {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            if self.used == 8 {
                self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = self.state;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                self.word = (z ^ (z >> 31)).to_le_bytes();
                self.used = 0;
            }
            *byte = self.word[self.used];
            self.used += 1;
        }
    }
}

#[test]
fn seed_stream_is_continuous_however_fills_are_split() {
    let mut whole = [0; 64];
    Seed::new(1).fill_bytes(&mut whole);
    for widths in [&[1, 7, 8, 3, 45][..], &[5, 5, 5, 49], &[64], &[13, 0, 51]] {
        let mut seed = Seed::new(1);
        let mut split = Vec::new();
        for &width in widths {
            let mut part = vec![0; width];
            seed.fill_bytes(&mut part);
            split.extend(part);
        }
        assert_eq!(split, whole, "{widths:?}");
    }
    let mut other = [0; 64];
    Seed::new(2).fill_bytes(&mut other);
    assert_ne!(other, whole);
}

/// Everything observable about one run, owned for comparison.
#[derive(Debug)]
struct Run {
    events: Vec<Event>,
    ids: Vec<MachineId>,
    stdout: [String; 4],
    stderr: [String; 4],
    /// Boot console of each machine, for diagnostics only.
    console: [String; 4],
    outcome: StepOutcome,
}

impl Run {
    fn oracle_exit(&self) -> Option<u8> {
        self.events.iter().find_map(|event| match &event.kind {
            EventKind::Output(output) if output.machine == self.ids[0] => match output.payload {
                Response::ApplicationOutcome { code } => Some(code),
                _ => None,
            },
            _ => None,
        })
    }
    /// Every machine's committed output, application outcomes, transitions
    /// and boot console tail, for failure diagnostics.
    fn report(&self) -> String {
        let mut text = format!("outcome {:?}\n", self.outcome);
        for (index, name) in NAMES.iter().enumerate() {
            let id = self.ids[index];
            text += &format!(
                "--- {name} stdout\n{}\n--- {name} stderr (last 4 KiB)\n{}\n",
                self.stdout[index],
                tail(&self.stderr[index], 4096)
            );
            text += &format!("--- {name} outcomes and transitions\n");
            for event in &self.events {
                match &event.kind {
                    EventKind::Output(output)
                        if output.machine == id
                            && matches!(output.payload, Response::ApplicationOutcome { .. }) =>
                    {
                        text += &format!("{} ns {:?}\n", event.at.as_nanos(), output.payload)
                    }
                    EventKind::Transition {
                        machine,
                        boot,
                        change,
                        ..
                    } if *machine == id => {
                        text += &format!("{} ns boot {boot} {change:?}\n", event.at.as_nanos())
                    }
                    _ => {}
                }
            }
            text += &format!(
                "--- {name} boot console (last 4 KiB)\n{}\n",
                tail(&self.console[index], 4096)
            );
        }
        text
    }
}

fn tail(text: &str, bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn run<M>(
    stage: &str,
    workers: usize,
    factory: impl FnMut(&MachineDefinition) -> Result<M, M::Error> + Send + 'static,
) -> Run
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let (oracle, nodes) = definitions(stage);
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        QUANTUM,
        workers,
        Seed::new(1),
        || Ok(()),
        oracle,
        nodes,
        Vec::new(),
        NetworkConfig::default(),
        factory,
    )
    .expect("driver construction");
    let ids: Vec<MachineId> = driver
        .with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect()))
        .unwrap();
    let mut run = Run {
        events: Vec::new(),
        ids,
        stdout: Default::default(),
        stderr: Default::default(),
        console: Default::default(),
        outcome: StepOutcome::BarrierReached {
            at: Timestamp::from_nanos(0),
        },
    };
    let guard = HangGuard::new("native etcd response transport must return", HOST_LIMIT);
    loop {
        let outcome = driver
            .step(Timestamp::from_nanos(LIMIT))
            .unwrap_or_else(|error| panic!("driver step: {error:?}\n{}", run.report()));
        for event in driver.drain_observations().events {
            if let EventKind::Output(output) = &event.kind {
                let index = run.ids.iter().position(|id| *id == output.machine).unwrap();
                let text = match &output.payload {
                    Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes,
                    } => Some((&mut run.stdout[index], bytes)),
                    Response::ApplicationOutput {
                        stream: Stream::Stderr,
                        bytes,
                    } => Some((&mut run.stderr[index], bytes)),
                    Response::BootConsole { bytes } => Some((&mut run.console[index], bytes)),
                    _ => None,
                };
                if let Some((text, bytes)) = text {
                    text.push_str(&String::from_utf8_lossy(bytes));
                }
            }
            run.events.push(event);
        }
        run.outcome = outcome.clone();
        match outcome {
            StepOutcome::BarrierReached { at } if at.as_nanos() < LIMIT => {}
            _ => break,
        }
        guard.remaining().unwrap_or_else(|error| {
            panic!(
                "etcd response infrastructure hang: {error}; {}",
                run.report()
            )
        });
    }
    run
}

/// Run `stage` on the backend `SEEDBOX_ETCD_BACKEND` selects.
fn on_backend(stage: &str, workers: usize) -> Run {
    let backend = std::env::var("SEEDBOX_ETCD_BACKEND").expect("set SEEDBOX_ETCD_BACKEND");
    let epoch = TEST_EPOCH_SECONDS;
    match backend.as_str() {
        "tcg" => {
            let mut factory = seedbox_machine_tcg::machine::tcg::Factory::new(
                &input("SEEDBOX_TCG_QEMU"),
                &input("SEEDBOX_TCG_ASSETS"),
                epoch,
            )
            .unwrap();
            run(stage, workers, move |definition| factory.create(definition))
        }
        #[cfg(seedbox_pv)]
        "pv" => {
            let mut factory = seedbox_machine_pv::machine::pv::Factory::new(
                &input("SEEDBOX_PV_WORKER"),
                &input("SEEDBOX_PV_ASSETS"),
                epoch,
            )
            .unwrap();
            run(stage, workers, move |definition| factory.create(definition))
        }
        other => panic!("unsupported SEEDBOX_ETCD_BACKEND {other:?} on this host"),
    }
}

/// S0: the cluster forms, the client sees one leader on every node, writes a
/// key through the leader and reads it back, linearizably, from a follower.
#[test]
#[ignore = "requires SEEDBOX_ETCD_BACKEND, its backend inputs, etcd and the etcd fixtures"]
fn cluster_acceptance() {
    assert_smoke(&on_backend("smoke", 1));
}

/// Repeat S0 at one worker, then four; compare every published event and
/// its timestamp without filtering application output or normalizing results.
#[test]
#[ignore = "requires SEEDBOX_ETCD_BACKEND, its backend inputs, etcd and the etcd fixtures"]
fn cluster_worker_repeatability() {
    let reference = on_backend("smoke", 1);
    assert_smoke(&reference);
    for workers in [1, 4] {
        let repeated = on_backend("smoke", workers);
        assert_smoke(&repeated);
        assert_eq!(reference.ids, repeated.ids, "workers={workers}");
        assert_eq!(reference.outcome, repeated.outcome, "workers={workers}");
        assert_eq!(
            reference.events.len(),
            repeated.events.len(),
            "event count differs, workers={workers}\n{}",
            repeated.report()
        );
        for (index, (expected, actual)) in reference.events.iter().zip(&repeated.events).enumerate()
        {
            assert_eq!(
                expected, actual,
                "first differing event {index}, workers={workers}"
            );
        }
        eprintln!(
            "etcd exact repeatability: workers={workers}, {} events",
            repeated.events.len()
        );
    }
}

fn assert_smoke(run: &Run) {
    let report = run.report();
    eprintln!(
        "etcd smoke: {} events, client {:?}, outcome {:?}",
        run.events.len(),
        run.stdout[0],
        run.outcome
    );
    assert_eq!(run.oracle_exit(), Some(0), "{report}");
    assert!(
        matches!(run.outcome, StepOutcome::Finished { .. }),
        "{report}"
    );
    let client = &run.stdout[0];
    assert!(client.contains("ETCDCLIENT ready attempts="), "{report}");
    assert!(client.contains("ETCDCLIENT leader node="), "{report}");
    assert!(
        client.contains("ETCDCLIENT put ok key=azA= revision="),
        "{report}"
    );
    assert!(client.contains(" key=azA= value=djA=\n"), "{report}");
    assert!(client.ends_with("ETCDCLIENT done\n"), "{report}");
    for node in 1..=3 {
        assert!(
            run.stdout[node].starts_with("ETCDNODE mounted /data\n"),
            "{report}"
        );
    }
}
