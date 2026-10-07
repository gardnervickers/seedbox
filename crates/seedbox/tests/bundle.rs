//! Contract tests use a test-only backend. No hardware qualification is implied.
use seedbox::{
    bundle::{self, Options},
    config::{self, Simulation},
    execution::*,
    image::Launch,
    orchestration::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use std::{
    collections::BTreeMap,
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Fixture {
    directory: tempfile::TempDir,
    simulation: Simulation,
    orchestrator: Orchestrator,
    preparer: Preparer,
    backend: Backend,
}

struct Preparer {
    inputs: PreparedInputs,
    calls: AtomicUsize,
}
impl PreparationBackend for Preparer {
    fn prepare(&self, _: &Simulation, _: &ContentStore) -> Result<PreparedInputs> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.inputs.clone())
    }
}

struct Backend {
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
    extra_logs: BTreeMap<String, Vec<u8>>,
    report: ExecutionReport,
    calls: AtomicUsize,
    fail: bool,
    cancel: bool,
}
impl ExecutionBackend for Backend {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.performance.clone()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for (digest, path) in request.artifacts {
            assert_eq!(ContentDigest::of(&fs::read(path)?), *digest);
        }
        assert_eq!(request.prepared.profile, self.profile);
        assert_eq!(request.performance_parameters, &self.performance);
        fs::write(
            request.output.join("workload.console"),
            format!(
                "seed={} timer_jitter={} stop_tolerance={}\n",
                request.seed, request.schedule.timer_jitter_ns, request.schedule.stop_tolerance_ns
            ),
        )?;
        for (name, bytes) in &self.extra_logs {
            let path = request.output.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, bytes)?;
        }
        if self.cancel {
            request.cancellation.cancel();
        }
        if self.fail {
            return Err(Error::Execution("injected backend failure".into()));
        }
        Ok(self.report.clone())
    }
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("compose.yaml");
        fs::write(&source, "name: test\nservices:\n  workload: {image: example/workload:v1}\nx-seedbox:\n  version: 1\n  seed: 1009\n  limits: {simulated_time: 10s, wall_time: 10s}\n  check: {service: workload}\n").unwrap();
        let simulation = config::load(&source, &BTreeMap::new()).unwrap();
        let store = ContentStore::new(directory.path().join("store"));
        let kernel = store.put(b"fixture kernel").unwrap();
        let engine = store.put(b"fixture engine").unwrap();
        let guest = store.put(b"fixture guest").unwrap();
        let manifest = store.put(b"fixture manifest").unwrap();
        let config = store.put(b"fixture image config").unwrap();
        let layer = store.put(b"fixture layer").unwrap();
        let profile = ExecutionProfile {
            backend: "test-only".into(),
            engine,
            witness_mode: WitnessMode::Exits,
            semantic_parameters: BTreeMap::from([("kernel_abi".into(), "v1".into())]),
        };
        let inputs = PreparedInputs {
            kernel,
            profile: profile.clone(),
            placement: BTreeMap::from([("workload".into(), "workload".into())]),
            guests: BTreeMap::from([("workload".into(), guest)]),
            images: BTreeMap::from([(
                "workload".into(),
                PreparedImage {
                    manifest,
                    config,
                    layers: vec![layer],
                    launch: Launch {
                        argv: vec!["/app".into()],
                        environment: BTreeMap::new(),
                        user: "".into(),
                        working_dir: "/".into(),
                        volumes: vec![],
                    },
                },
            )]),
            auxiliary: BTreeMap::from([(
                "bootstrap".into(),
                store.put(b"fixture auxiliary").unwrap(),
            )]),
            provenance: BTreeMap::from([("builder".into(), "fixture-v1".into())]),
        };
        let backend = Backend {
            profile,
            performance: BTreeMap::from([("SEEDBOX_UNDO".into(), "256".into())]),
            extra_logs: BTreeMap::new(),
            report: ExecutionReport {
                reason: StopReason::Completed,
                workload_exit: Some(WorkloadExit {
                    service: "workload".into(),
                    code: 0,
                }),
                witness: Some(ContentDigest::of(b"witness")),
                full_state: None,
                faults: vec![],
                detail: None,
                engine_qualification: None,
            },
            calls: AtomicUsize::new(0),
            fail: false,
            cancel: false,
        };
        let orchestrator = Orchestrator::new(store, directory.path().join("runs"));
        Self {
            directory,
            simulation,
            orchestrator,
            preparer: Preparer {
                inputs,
                calls: AtomicUsize::new(0),
            },
            backend,
        }
    }
    fn run(&self) -> Run {
        self.orchestrator
            .run(
                &self.simulation,
                RunOptions::default(),
                &self.preparer,
                &self.backend,
            )
            .unwrap()
    }
}

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};
fn export(f: &Fixture, run: &Run) -> PathBuf {
    let path = f.directory.path().join("run.tar");
    bundle::export(
        &run.directory,
        &ContentStore::new(f.directory.path().join("store")),
        &path,
        &Options::default(),
    )
    .unwrap();
    path
}
struct Entry {
    header: tar::Header,
    bytes: Vec<u8>,
}
fn entries(path: &Path) -> Vec<Entry> {
    tar::Archive::new(fs::File::open(path).unwrap())
        .entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let header = e.header().clone();
            let mut bytes = vec![];
            e.read_to_end(&mut bytes).unwrap();
            Entry { header, bytes }
        })
        .collect()
}
fn write_entries(path: &Path, entries: &mut [Entry]) {
    let mut f = fs::File::create(path).unwrap();
    for e in entries {
        e.header.set_size(e.bytes.len() as u64);
        e.header.set_cksum();
        f.write_all(e.header.as_bytes()).unwrap();
        f.write_all(&e.bytes).unwrap();
        f.write_all(&[0u8; 512][..((512 - e.bytes.len() % 512) % 512)])
            .unwrap();
    }
    f.write_all(&[0u8; 1024]).unwrap();
}
fn modify_record(entries: &mut [Entry], change: impl FnOnce(&mut RunRecord)) {
    let manifest: serde_json::Value = serde_json::from_slice(&entries[1].bytes).unwrap();
    let mut record: RunRecord = serde_json::from_value(manifest["record"].clone()).unwrap();
    change(&mut record);
    entries[1].bytes = serde_json::to_vec(&serde_json::json!({"sha256": ContentDigest::of(&serde_json::to_vec(&record).unwrap()), "record":record})).unwrap();
    let mut env: serde_json::Value = serde_json::from_slice(&entries[0].bytes).unwrap();
    env["manifest"] = serde_json::json!({"digest": ContentDigest::of(&entries[1].bytes), "bytes":entries[1].bytes.len()});
    entries[0].bytes = serde_json::to_vec(&env).unwrap();
}
fn rejected(bundle: &Path, state: &Path, options: &Options) {
    assert!(bundle::import(bundle, state, options).is_err());
    if state.join("runs").is_dir() {
        assert_eq!(fs::read_dir(state.join("runs")).unwrap().count(), 0);
    }
    if state.is_dir() {
        assert!(!fs::read_dir(state).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".bundle-")));
    }
}

#[test]
fn detached_round_trip_preserves_bytes_record_and_replays_failure_without_preparation() {
    for code in [0, 7] {
        let mut f = Fixture::new();
        f.backend.report.workload_exit.as_mut().unwrap().code = code;
        let opaque = vec![0, 255, 13, 10, 128, 0, 42];
        f.backend
            .extra_logs
            .insert("nested/opaque.bin".into(), opaque.clone());
        let run = f.run();
        let original_manifest = fs::read(run.directory.join("manifest.json")).unwrap();
        let archive = export(&f, &run);
        let bytes = fs::read(&archive).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let archive = dst.path().join("saved.tar");
        fs::write(&archive, bytes).unwrap();
        fs::remove_dir_all(f.directory.path()).unwrap();
        let state = dst.path().join("fresh");
        let imported = bundle::import(&archive, &state, &Options::default()).unwrap();
        assert_eq!(inspect(&imported.directory).unwrap(), run.record);
        assert_eq!(
            fs::read(imported.directory.join("manifest.json")).unwrap(),
            original_manifest
        );
        assert_eq!(
            fs::read(imported.directory.join("output/nested/opaque.bin")).unwrap(),
            opaque
        );
        let store = ContentStore::new(state.join("objects"));
        for digest in run
            .record
            .identity
            .inputs
            .digests()
            .chain(std::iter::once(&run.record.run_id))
            .chain(run.record.logs.values())
        {
            store.verify(digest).unwrap();
        }
        let fresh = Orchestrator::new(store, state.join("runs"))
            .replay(
                &imported.directory,
                &f.backend,
                &CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(fresh.record.outcome, run.record.outcome);
        assert_eq!(fresh.record.replay.unwrap().status, ReplayStatus::Match);
        assert_eq!(f.preparer.calls.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn omissions_and_unfinished_results_survive_without_invented_files() {
    for reason in [StopReason::Error, StopReason::Cancelled] {
        let mut f = Fixture::new();
        f.backend.report.reason = reason;
        f.orchestrator = f.orchestrator.with_log_policy(LogPolicy {
            max_file_bytes: 0,
            max_total_bytes: 0,
        });
        let run = f.run();
        let archive = export(&f, &run);
        let state = f.directory.path().join("new");
        let imported = bundle::import(&archive, &state, &Options::default()).unwrap();
        assert_eq!(imported.record, run.record);
        assert!(!imported.record.omitted_logs.is_empty());
        assert_eq!(
            fs::read_dir(imported.directory.join("output"))
                .unwrap()
                .count(),
            0
        );
    }
}
#[test]
fn every_declared_object_is_required_and_hash_checked() {
    let f = Fixture::new();
    let run = f.run();
    let archive = export(&f, &run);
    let good = entries(&archive);
    for i in 2..good.len() {
        let mut bad = entries(&archive);
        bad[i].bytes[0] ^= 1;
        let path = f.directory.path().join("bad.tar");
        write_entries(&path, &mut bad);
        rejected(
            &path,
            &f.directory.path().join(format!("corrupt-{i}")),
            &Options::default(),
        );
        let mut bad = entries(&archive);
        bad.remove(i);
        write_entries(&path, &mut bad);
        rejected(
            &path,
            &f.directory.path().join(format!("missing-{i}")),
            &Options::default(),
        );
    }
    let store = ContentStore::new(f.directory.path().join("store"));
    let object = run.record.identity.inputs.kernel.hex();
    fs::write(
        f.directory.path().join("store/sha256").join(object),
        b"broken",
    )
    .unwrap();
    let output = f.directory.path().join("never-published.tar");
    assert!(bundle::export(&run.directory, &store, &output, &Options::default()).is_err());
    assert!(!output.exists());
}
#[test]
fn manifest_identity_formats_duplicates_and_trailing_bytes_are_rejected() {
    let f = Fixture::new();
    let archive = export(&f, &f.run());
    for case in 0..8 {
        let mut e = entries(&archive);
        match case {
            0 => e[1].bytes[0] ^= 1,
            1 => modify_record(&mut e, |r| r.run_id = ContentDigest::of(b"wrong identity")),
            2 => modify_record(&mut e, |r| {
                r.identity.version = 999;
                r.run_id = ContentDigest::of(&serde_json::to_vec(&r.identity).unwrap());
            }),
            3 => {
                let mut env: serde_json::Value = serde_json::from_slice(&e[0].bytes).unwrap();
                env["version"] = 999.into();
                e[0].bytes = serde_json::to_vec(&env).unwrap();
            }
            4 => {
                let duplicate = Entry {
                    header: e[2].header.clone(),
                    bytes: e[2].bytes.clone(),
                };
                e.insert(2, duplicate);
            }
            5 => {
                let mut env: serde_json::Value = serde_json::from_slice(&e[0].bytes).unwrap();
                let duplicate = env["objects"][0].clone();
                env["objects"].as_array_mut().unwrap().push(duplicate);
                e[0].bytes = serde_json::to_vec(&env).unwrap();
            }
            6 => e[2].header.as_mut_bytes()[0] ^= 1,
            _ => (),
        }
        let path = f.directory.path().join(format!("case-{case}.tar"));
        write_entries(&path, &mut e);
        if case == 7 {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"extra")
                .unwrap();
        }
        rejected(
            &path,
            &f.directory.path().join(format!("state-{case}")),
            &Options::default(),
        );
    }
    let bytes = fs::read(&archive).unwrap();
    for cut in [1, 511, bytes.len() - 1] {
        let path = f.directory.path().join("truncated.tar");
        fs::write(&path, &bytes[..cut]).unwrap();
        rejected(
            &path,
            &f.directory.path().join(format!("cut-{cut}")),
            &Options::default(),
        );
    }
}
#[test]
fn archive_types_and_noncanonical_paths_cannot_escape_staging() {
    let f = Fixture::new();
    let archive = export(&f, &f.run());
    for (i, kind) in b"123456xgLS".iter().copied().enumerate() {
        let mut e = entries(&archive);
        e[2].header.set_entry_type(tar::EntryType::new(kind));
        let path = f.directory.path().join("bad-type.tar");
        write_entries(&path, &mut e);
        rejected(
            &path,
            &f.directory.path().join(format!("kind-{i}")),
            &Options::default(),
        );
    }
    for (i, name) in [
        "../outside",
        "/outside",
        "objects/sha256/../outside",
        "objects//sha256/name",
    ]
    .into_iter()
    .enumerate()
    {
        let mut e = entries(&archive);
        e[2].header.as_mut_bytes()[..100].fill(0);
        e[2].header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        let path = f.directory.path().join("bad-name.tar");
        write_entries(&path, &mut e);
        rejected(
            &path,
            &f.directory.path().join(format!("name-{i}")),
            &Options::default(),
        );
    }
    assert!(!f.directory.path().join("outside").exists());
}
#[test]
fn log_paths_prefix_collisions_and_alias_amplification_fail_before_state_creation() {
    let mut f = Fixture::new();
    f.backend
        .extra_logs
        .insert("large.bin".into(), vec![7; 8192]);
    let archive = export(&f, &f.run());
    for (i, name) in [
        "../outside",
        "/outside",
        "a//b",
        "a/./b",
        "a/../b",
        "a\\b",
        "",
    ]
    .into_iter()
    .enumerate()
    {
        let mut e = entries(&archive);
        modify_record(&mut e, |r| {
            let digest = r.logs["large.bin"].clone();
            r.logs.insert(name.into(), digest);
        });
        let path = f.directory.path().join("bad-log.tar");
        write_entries(&path, &mut e);
        let state = f.directory.path().join(format!("log-{i}"));
        rejected(&path, &state, &Options::default());
        assert!(!state.exists());
    }
    let mut e = entries(&archive);
    modify_record(&mut e, |r| {
        let d = r.logs["large.bin"].clone();
        r.logs.insert("a".into(), d.clone());
        r.logs.insert("a/b".into(), d);
    });
    let path = f.directory.path().join("prefix.tar");
    write_entries(&path, &mut e);
    rejected(
        &path,
        &f.directory.path().join("prefix"),
        &Options::default(),
    );
    let mut e = entries(&archive);
    modify_record(&mut e, |r| {
        let d = r.logs["large.bin"].clone();
        for i in 0..16 {
            r.logs.insert(format!("alias-{i}"), d.clone());
        }
    });
    let env: serde_json::Value = serde_json::from_slice(&e[0].bytes).unwrap();
    let unique: u64 = env["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["bytes"].as_u64().unwrap())
        .sum();
    let path = f.directory.path().join("aliases.tar");
    write_entries(&path, &mut e);
    let state = f.directory.path().join("aliases");
    let options = Options {
        max_bytes: unique,
        ..Default::default()
    };
    rejected(&path, &state, &options);
    assert!(!state.exists());
    let manifest: serde_json::Value = serde_json::from_slice(&e[1].bytes).unwrap();
    let aliases: RunRecord = serde_json::from_value(manifest["record"].clone()).unwrap();
    let lengths: BTreeMap<_, _> = env["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["digest"].as_str().unwrap(), o["bytes"].as_u64().unwrap()))
        .collect();
    let materialized: u64 = aliases.logs.values().map(|d| lengths[d.hex()]).sum();
    let exact = Options {
        max_bytes: materialized,
        ..Default::default()
    };
    bundle::import(&path, &f.directory.path().join("alias-exact"), &exact).unwrap();
    rejected(
        &path,
        &f.directory.path().join("alias-over"),
        &Options {
            max_bytes: materialized - 1,
            ..Default::default()
        },
    );
    let mut e = entries(&archive);
    modify_record(&mut e, |r| {
        let d = r.logs["large.bin"].clone();
        for i in 0..8193 {
            r.logs.insert(format!("name-{i}"), d.clone());
        }
    });
    let path = f.directory.path().join("names.tar");
    write_entries(&path, &mut e);
    let state = f.directory.path().join("names");
    rejected(&path, &state, &Options::default());
    assert!(!state.exists());
}
#[cfg(unix)]
#[test]
fn symlinked_state_root_components_and_objects_never_modify_outside_storage() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let run = f.run();
    let archive = export(&f, &run);
    for (i, component) in ["root", "objects", "objects/sha256", "runs"]
        .into_iter()
        .enumerate()
    {
        for dangling in [false, true] {
            let outside = f.directory.path().join(format!("outside-{i}-{dangling}"));
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("sentinel"), b"untouched").unwrap();
            let target = if dangling {
                outside.join("missing")
            } else {
                outside.clone()
            };
            let state = f.directory.path().join(format!("state-{i}-{dangling}"));
            if component == "root" {
                symlink(&target, &state).unwrap();
            } else {
                let link = state.join(component);
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                symlink(&target, link).unwrap();
            }
            assert!(bundle::import(&archive, &state, &Options::default()).is_err());
            if component == "root" && !dangling {
                let trailing = PathBuf::from(format!("{}/", state.display()));
                assert!(bundle::import(&archive, &trailing, &Options::default()).is_err());
                assert!(bundle::import(&archive, &state.join("."), &Options::default()).is_err());
            }
            assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        }
    }
    let state = f.directory.path().join("object-link");
    fs::create_dir_all(state.join("objects/sha256")).unwrap();
    let target = f.directory.path().join("sentinel");
    fs::write(&target, b"untouched").unwrap();
    symlink(
        &target,
        state
            .join("objects/sha256")
            .join(run.record.identity.inputs.kernel.hex()),
    )
    .unwrap();
    rejected(&archive, &state, &Options::default());
    assert_eq!(fs::read(target).unwrap(), b"untouched");
}
#[test]
fn existing_corruption_output_collision_cancellation_and_transfer_limits_are_fail_closed() {
    let f = Fixture::new();
    let run = f.run();
    let archive = export(&f, &run);
    let store = ContentStore::new(f.directory.path().join("store"));
    let original = fs::read(&archive).unwrap();
    assert!(bundle::export(&run.directory, &store, &archive, &Options::default()).is_err());
    assert_eq!(fs::read(&archive).unwrap(), original);
    let state = f.directory.path().join("corrupt");
    let dest = ContentStore::new(state.join("objects"));
    let d = dest.put(b"fixture kernel").unwrap();
    let path = state.join("objects/sha256").join(d.hex());
    fs::write(&path, b"broken").unwrap();
    rejected(&archive, &state, &Options::default());
    assert_eq!(fs::read(path).unwrap(), b"broken");
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let options = Options {
        cancellation,
        ..Default::default()
    };
    let state = f.directory.path().join("cancelled");
    rejected(&archive, &state, &options);
    assert!(!state.exists());
    let e = entries(&archive);
    let env: serde_json::Value = serde_json::from_slice(&e[0].bytes).unwrap();
    let total: u64 = env["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["bytes"].as_u64().unwrap())
        .sum();
    let exact = Options {
        max_bytes: total,
        ..Default::default()
    };
    bundle::import(&archive, &f.directory.path().join("exact"), &exact).unwrap();
    rejected(
        &archive,
        &f.directory.path().join("too-small"),
        &Options {
            max_bytes: total - 1,
            ..Default::default()
        },
    );
    let mut bytes = fs::read(&archive).unwrap();
    let mut h = tar::Header::from_byte_slice(&bytes[..512]).clone();
    h.set_size(8 * 1024 * 1024 + 1);
    h.set_cksum();
    bytes[..512].copy_from_slice(h.as_bytes());
    let path = f.directory.path().join("oversized.tar");
    fs::write(&path, bytes).unwrap();
    rejected(
        &path,
        &f.directory.path().join("metadata"),
        &Options::default(),
    );
}

#[test]
fn zero_byte_aliases_still_consume_the_materialized_path_limit() {
    let mut f = Fixture::new();
    f.backend.extra_logs.insert("empty".into(), vec![]);
    let archive = export(&f, &f.run());
    let mut e = entries(&archive);
    modify_record(&mut e, |r| {
        let d = r.logs["empty"].clone();
        for i in 0..8193 {
            r.logs.insert(format!("empty-{i}"), d.clone());
        }
    });
    let path = f.directory.path().join("zero-aliases.tar");
    write_entries(&path, &mut e);
    let state = f.directory.path().join("zero-aliases");
    rejected(&path, &state, &Options::default());
    assert!(!state.exists());
}
#[test]
fn portable_cli_default_export_import_and_inspect_need_no_tools_or_original_state() {
    use std::process::Command;
    let mut f = Fixture::new();
    f.backend
        .extra_logs
        .insert("opaque".into(), vec![0, 255, 128]);
    let run = f.run();
    fs::rename(
        f.directory.path().join("store"),
        f.directory.path().join("objects"),
    )
    .unwrap();
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_seedbox"))
            .args(args)
            .current_dir(f.directory.path())
            .env_clear()
            .env("HOME", f.directory.path())
            .env("PATH", "")
            .output()
            .unwrap()
    };
    let out = invoke(&[
        "export",
        run.directory.to_str().unwrap(),
        "--state",
        f.directory.path().to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let name = format!(
        "{}.seedbox.tar",
        run.directory.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("EXPORTED: {name}\n")
    );
    fs::remove_dir_all(f.directory.path().join("objects")).unwrap();
    fs::remove_dir_all(f.directory.path().join("runs")).unwrap();
    fs::remove_file(&f.simulation.source).unwrap();
    let out = invoke(&["import", &name, "--state", "fresh"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .strip_prefix("IMPORTED: ")
        .unwrap()
        .to_owned();
    let out = invoke(&["inspect", &path, "--json"]);
    assert!(out.status.success());
    let record: RunRecord = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record, run.record);
    let collision = invoke(&["export", &path, "--state", "fresh", "-o", &name]);
    assert_eq!(collision.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&collision.stderr),
        format!("error: --output {name}: already exists; choose a new file\n")
    );
}

#[test]
fn envelope_missing_unrelated_and_over_count_objects_fail_before_opening_state() {
    let f = Fixture::new();
    let archive = export(&f, &f.run());
    for case in 0..3 {
        let mut e = entries(&archive);
        let mut envelope: serde_json::Value = serde_json::from_slice(&e[0].bytes).unwrap();
        match case {
            0 => {
                envelope["objects"].as_array_mut().unwrap().pop();
            }
            1 => {
                envelope["objects"][0]["digest"] =
                    serde_json::to_value(ContentDigest::of(b"unrelated")).unwrap();
            }
            _ => {
                let object = envelope["objects"][0].clone();
                envelope["objects"] = serde_json::Value::Array(vec![object; 8193]);
            }
        }
        e[0].bytes = serde_json::to_vec(&envelope).unwrap();
        let path = f.directory.path().join(format!("inventory-{case}.tar"));
        write_entries(&path, &mut e);
        let state = f.directory.path().join(format!("inventory-{case}"));
        rejected(&path, &state, &Options::default());
        assert!(!state.exists());
    }
}

/// Exercises real artifact admission with synthetic execution, never host probes.
struct DevBackend<'a> {
    policy: seedbox::vmm::VmmExecutor,
    fixture: &'a Backend,
}
impl ExecutionBackend for DevBackend<'_> {
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.fixture.performance_parameters()
    }
    fn profile(&self) -> ExecutionProfile {
        self.policy.profile()
    }
    fn validate_retained(&self, inputs: &PreparedInputs) -> Result<()> {
        self.policy.validate_retained(inputs)
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        self.fixture.execute(request)
    }
}

#[test]
fn dev_replay_permission_exact_hashes_and_bundle_marker_round_trip() {
    let mut f = Fixture::new();
    let store = ContentStore::new(f.directory.path().join("store"));
    let engine = store
        .materialize(
            &f.backend.profile.engine,
            &f.directory.path().join("engine"),
        )
        .unwrap();
    let policy = seedbox::vmm::VmmExecutor::new(
        &engine,
        &store,
        None,
        512,
        WitnessMode::Exits,
        seedbox::counter::CounterProfile::Zen,
    )
    .unwrap();
    let dev = policy
        .clone()
        .with_dev_engine(true, &f.preparer.inputs.kernel);
    f.backend.profile = dev.profile();
    f.preparer.inputs.profile = dev.profile();
    let backend = DevBackend {
        policy: dev.clone(),
        fixture: &f.backend,
    };
    let run = f
        .orchestrator
        .run(&f.simulation, RunOptions::default(), &f.preparer, &backend)
        .unwrap();
    assert_eq!(run.record.outcome, Outcome::Pass);
    assert_eq!(
        run.record.identity.inputs.profile.semantic_parameters["engine_qualification"],
        "dev-unpinned"
    );
    assert_eq!(
        run.record
            .report
            .as_ref()
            .unwrap()
            .engine_qualification
            .as_deref(),
        Some("dev-unpinned")
    );
    let baseline_manifest = fs::read(run.directory.join("manifest.json")).unwrap();
    let archive = export(&f, &run);
    let state = f.directory.path().join("imported");
    let imported = bundle::import(&archive, &state, &Options::default()).unwrap();
    assert_eq!(inspect(&imported.directory).unwrap(), run.record);
    let runner = Orchestrator::new(ContentStore::new(state.join("objects")), state.join("runs"));
    let cli_refusal = std::process::Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args([
            "replay",
            imported.directory.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ])
        .env_clear()
        .env("HOME", f.directory.path())
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(cli_refusal.status.code(), Some(2));
    let stdout = String::from_utf8(cli_refusal.stdout).unwrap();
    assert!(stdout.starts_with("ERROR: "));
    assert!(stdout.contains("replay: NOT_COMPARABLE (unqualified dev engine)"));
    assert!(String::from_utf8(cli_refusal.stderr)
        .unwrap()
        .contains("--allow-dev-engine"));
    let cli_inspect = std::process::Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(["inspect", imported.directory.to_str().unwrap(), "--json"])
        .env_clear()
        .env("HOME", f.directory.path())
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(cli_inspect.status.success());
    let inspected: serde_json::Value = serde_json::from_slice(&cli_inspect.stdout).unwrap();
    assert_eq!(
        inspected["identity"]["inputs"]["profile"]["semantic_parameters"]["engine_qualification"],
        "dev-unpinned"
    );
    assert_eq!(inspected["report"]["engine_qualification"], "dev-unpinned");

    let matched = runner
        .replay(&imported.directory, &backend, &CancellationToken::default())
        .unwrap();
    assert_eq!(matched.record.replay.unwrap().status, ReplayStatus::Match);
    assert_eq!(matched.record.identity, run.record.identity);
    let refused_backend = DevBackend {
        policy,
        fixture: &f.backend,
    };
    let refused = runner
        .replay(
            &imported.directory,
            &refused_backend,
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(
        refused.record.replay.unwrap().status,
        ReplayStatus::NotComparable
    );
    assert!(refused.record.error.unwrap().contains("--allow-dev-engine"));
    assert!(!refused.directory.join("output").exists());
    let changed_engine = f.directory.path().join("changed-engine");
    fs::write(&changed_engine, b"different dev engine").unwrap();
    let changed = seedbox::vmm::VmmExecutor::new(
        &changed_engine,
        &store,
        None,
        512,
        WitnessMode::Exits,
        seedbox::counter::CounterProfile::Zen,
    )
    .unwrap()
    .with_dev_engine(true, &f.preparer.inputs.kernel);
    let changed_backend = DevBackend {
        policy: changed,
        fixture: &f.backend,
    };
    for allow_engine_change in [false, true] {
        let rejected = runner
            .replay_with_options(
                &imported.directory,
                &changed_backend,
                ReplayOptions {
                    allow_engine_change,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            rejected.record.replay.unwrap().status,
            ReplayStatus::NotComparable
        );
        assert!(!rejected.directory.join("output").exists());
    }
    f.backend.calls.store(0, Ordering::SeqCst);
    let mut mismatching = f.backend.report.clone();
    mismatching.witness = Some(ContentDigest::of(b"different witness"));
    let mismatch_fixture = Backend {
        report: mismatching,
        ..f.backend
    };
    let mismatch_backend = DevBackend {
        policy: dev,
        fixture: &mismatch_fixture,
    };
    let mismatch = runner
        .replay(
            &imported.directory,
            &mismatch_backend,
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(
        mismatch.record.replay.unwrap().status,
        ReplayStatus::Mismatch
    );
    assert_eq!(
        mismatch
            .record
            .report
            .unwrap()
            .engine_qualification
            .as_deref(),
        Some("dev-unpinned")
    );
    assert_eq!(
        fs::read(run.directory.join("manifest.json")).unwrap(),
        baseline_manifest
    );
    assert_eq!(
        fs::read(imported.directory.join("manifest.json")).unwrap(),
        baseline_manifest
    );
}
