//! Real CLI → ordinary OCI preparation → retained Driver → native PV acceptance.
//! Never substitutes a prepared manifest, executor, or execution report.
mod interactions;

use seedbox_driver_execution::process_lifetime::{reclaim_barrier, EXECUTION_LEASE_FILE};
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

struct Service {
    root: Option<tempfile::TempDir>,
    child: Option<Child>,
    binary: PathBuf,
    docker: std::ffi::OsString,
    backend: &'static str,
    inherit_environment: bool,
    asset_directory: Option<PathBuf>,
}
impl Service {
    fn new(backend: &'static str) -> Self {
        Self::start(backend, false, false)
    }
    fn start(backend: &'static str, inherit_environment: bool, relocate_sdk: bool) -> Self {
        let root = tempfile::Builder::new()
            .prefix("efd-")
            .tempdir_in("/tmp")
            .unwrap();
        let binary = std::env::var_os("SEEDBOX_TEST_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("CARGO_TARGET_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
                    .join("release/seedbox")
            })
            .canonicalize()
            .expect("normal cargo xtask build-assets and cargo build --release first");
        let docker = std::env::var_os("DOCKER_HOST")
            .unwrap_or_else(|| "unix:///run/podman/podman.sock".into());
        let asset_directory = relocate_sdk.then(|| {
            let directory = root.path().join("relocated-sdk");
            let architecture = directory.join(std::env::consts::ARCH);
            fs::create_dir_all(&architecture).unwrap();
            fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../build/assets/driver-runtime.tar.gz"),
                architecture.join("driver-runtime.tar.gz"),
            )
            .unwrap();
            directory
        });
        let mut this = Self {
            root: Some(root),
            child: None,
            binary,
            docker,
            backend,
            inherit_environment,
            asset_directory,
        };
        let log = fs::File::create(this.path().join("server.log")).unwrap();
        let child = this
            .command()
            .args(["server", "start", "--foreground"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .guarded_spawn()
            .unwrap();
        this.child = Some(child);
        HangGuard::new(
            "public daemon must publish its listening socket",
            Duration::from_secs(60),
        )
        .wait_for(|| {
            assert!(
                this.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "service exited: {}",
                fs::read_to_string(this.path().join("server.log")).unwrap()
            );
            std::os::unix::net::UnixStream::connect(this.path().join("state/server.sock")).ok()
        })
        .expect("daemon socket readiness infrastructure hang");
        this
    }
    fn path(&self) -> &Path {
        self.root.as_ref().unwrap().path()
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        if !self.inherit_environment {
            command
                .env_clear()
                .env("DOCKER_HOST", &self.docker)
                .env("PATH", "");
        } else {
            // Removed overrides must not select or launch a legacy runtime.
            command.env("SEEDBOX_MKE2FS", self.path().join("unused-legacy-mke2fs"));
        }
        if let Some(directory) = &self.asset_directory {
            command.env("SEEDBOX_ASSET_DIR", directory);
        }
        command
            .env("HOME", self.path())
            .env("TMPDIR", self.path())
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env("SEEDBOX_STATE", self.path().join("state"))
            .current_dir(self.path());
        command
    }
    fn cli(&self, args: &[&str]) -> Output {
        let output = self.command().args(args).guarded_output().unwrap();
        if !output.status.success() {
            // Catalogue state disappears during graceful daemon/session cleanup.
            // Retain the actual public resource error before Drop tears it down.
            let diagnostic = self
                .command()
                .args(["show", "--json"])
                .guarded_output()
                .unwrap();
            fs::write(
                self.path().join("failed-catalogue.json"),
                &diagnostic.stdout,
            )
            .unwrap();
            eprintln!(
                "public failure catalogue: {}",
                String::from_utf8_lossy(&diagnostic.stdout)
            );
        }
        assert!(
            output.status.success(),
            "CLI {args:?}: status={} stdout={} stderr={}\nserver={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(self.path().join("server.log")).unwrap()
        );
        output
    }
    fn run(&self, project: &Path) -> String {
        let mut args = vec![
            "run",
            project.to_str().unwrap(),
            "--seed",
            "1009",
            "--wall-time",
            "15m",
            "--json",
        ];
        // Preserve actual coverage of omission's PV default, not just --backend pv.
        if self.backend != "pv" {
            args.extend(["--backend", self.backend]);
        }
        let output = self.cli(&args);
        let run: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let id = run["metadata"]["id"].as_str().unwrap().to_owned();
        self.assert_profile(&id);
        id
    }
    fn assert_profile(&self, id: &str) {
        let output = self.cli(&["show", "--view", "definition", id, "--json"]);
        let run: seedbox_api::Simulation = serde_json::from_slice(&output.stdout).unwrap();
        let seedbox_api::InputSource::Resolved { definition } = &run.spec.inputs else {
            panic!("completed ordinary run must retain a resolved definition")
        };
        let execution = &definition.runtime.execution;
        assert_eq!(
            execution["backend"],
            format!("driver-{}", self.backend),
            "{execution}"
        );
        assert_eq!(execution["backend"], format!("driver-{}", self.backend));
        eprintln!("normal CLI retained profile: id={id} execution={execution}");
    }
    fn witness(&self, id: &str) -> String {
        let reports = directories(&self.path().join("state/sessions"))
            .into_iter()
            .filter(|path| {
                path.components()
                    .any(|component| component.as_os_str() == id)
            })
            .map(|path| path.join("vmm.json"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 1);
        let report: seedbox_application::execution::ExecutionReport =
            serde_json::from_reader(fs::File::open(&reports[0]).unwrap()).unwrap();
        let witness = report
            .witness
            .expect("actual retained worker must supply native evidence")
            .hex()
            .to_owned();
        eprintln!("normal CLI raw native witness: id={id} witness={witness}");
        witness
    }
    fn replay(&self, id: &str) {
        let original = self.witness(id);
        let different = if self.backend == "pv" { "tcg" } else { "pv" };
        let denied = self
            .command()
            .args(["replay", id, "--backend", different, "--json"])
            .guarded_output()
            .unwrap();
        assert_eq!(denied.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&denied.stderr).contains("differs from retained backend"));
        let output = self.cli(&["replay", id, "--wall-time", "15m", "--json"]);
        let comparison: seedbox_api::SimulationComparison =
            serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(comparison.status, seedbox_api::ComparisonStatus::Match);
        assert_eq!(comparison.witness_matches, Some(true));
        self.assert_profile(comparison.simulation.as_str());
        assert_eq!(
            original,
            self.witness(comparison.simulation.as_str()),
            "raw native witnesses must match"
        );
        let value = serde_json::to_value(&comparison).unwrap();
        eprintln!("normal CLI replay MATCH: {value}");
        // Public comparison is backed by the normal worker's native witness,
        // not by equality of preparation metadata or a canned executor report.
        assert_eq!(original.len(), 64);
        let output = self.cli(&[
            "run",
            "--from",
            id,
            "--backend",
            self.backend,
            "--wall-time",
            "15m",
            "--json",
        ]);
        let rerun: seedbox_api::Simulation = serde_json::from_slice(&output.stdout).unwrap();
        self.assert_profile(rerun.metadata.id.as_str());
        assert_eq!(original, self.witness(rerun.metadata.id.as_str()));
        eprintln!(
            "normal CLI same-backend rerun: backend={} id={}",
            self.backend, rerun.metadata.id
        );
    }
    fn cleanup_barriers(&self) -> Vec<fs::File> {
        let dirs = directories(self.path());
        let mut barriers = Vec::new();
        for directory in &dirs {
            let name = directory.file_name().unwrap().to_string_lossy();
            assert!(
                !name.starts_with("native-scratch")
                    && !name.starts_with("seedbox-pv-")
                    && !name.starts_with("driver-runtime-"),
                "native inputs survive public completion: {directory:?}"
            );
            let lease = directory.join(EXECUTION_LEASE_FILE);
            if lease.exists() {
                barriers.push(reclaim_barrier(&lease, Duration::ZERO).unwrap());
            }
        }
        eprintln!("normal CLI pre-close W2: native/runtime absent with execution lease barriers held; live server still owns the session lease");
        barriers
    }
    fn close(mut self) {
        // The live production server deliberately owns a shared session lease.
        // Prove execution quiescence before close, then release our barriers so
        // its normal shutdown can acquire ALL barriers and remove its session.
        {
            let _barriers = self.cleanup_barriers();
        }
        let mut child = self.child.take().unwrap();
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read_dir(self.path().join("state/sessions"))
                .unwrap()
                .count(),
            0,
            "production shutdown must reap and reclaim the owned session"
        );
        self.root.take().unwrap().close().unwrap();
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let guard = HangGuard::new(
                "failed-case daemon must finish graceful shutdown",
                Duration::from_secs(30),
            );
            if let Err(error) =
                guard.wait_for(|| child.try_wait().expect("observe failed-case daemon exit"))
            {
                eprintln!("cleanup infrastructure hang: {error}; forcing daemon termination");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some(root) = self.root.take() {
            eprintln!(
                "failed normal CLI proof retained inputs at {}",
                root.keep().display()
            );
        }
    }
}
fn directories(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        if !path.exists() {
            continue;
        }
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                found.push(entry.path());
                pending.push(entry.path());
            }
        }
    }
    found
}

#[test]
fn normal_driver_inherited_environment_relocated_sdk_run_and_replay_match() {
    // The example, default-PV selection, ordinary Nix environment and relocated
    // SDK all exercise the same expensive native preparation and replay path.
    existing_example_service(Service::start("pv", true, true));
}
#[test]
fn normal_driver_tcg_existing_example_run_and_replay_match() {
    existing_example("tcg");
}
fn existing_example(backend: &'static str) {
    existing_example_service(Service::new(backend));
}
fn existing_example_service(service: Service) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/redis")
        .canonicalize()
        .unwrap();
    let project = service.path().join("example");
    copy_project(&source, &project);
    let id = service.run(&project.join("seedbox.yaml"));
    fs::rename(&project, service.path().join("source-no-longer-available")).unwrap();
    service.replay(&id);
    service.close();
}

#[test]
fn normal_driver_two_guests_only_the_oracle_can_use_control() {
    two_guest_authority("pv");
}
#[test]
fn normal_driver_tcg_two_guests_only_the_oracle_can_use_control() {
    two_guest_authority("tcg");
}
fn two_guest_authority(backend: &'static str) {
    let service = Service::new(backend);
    let oracle = r#"import socket,json
listener=socket.socket(); listener.bind(('0.0.0.0',31415)); listener.listen(1); listener.settimeout(30)
channel=socket.socket(socket.AF_UNIX); channel.connect('/run/seedbox/control.sock'); reader=channel.makefile('rb')
def now(i):
 channel.sendall((json.dumps(dict(version=1,id=i,op='now'))+'\n').encode()); result=json.loads(reader.readline()); assert result['version']==1 and result['ok'] and result['id']==i and result['result']['now_ns']==result['effect_at_ns']; print('CONTROLLER_ACK',json.dumps(result),flush=True)
now(1)
peer,_=listener.accept(); peer.settimeout(30); assert peer.makefile('rb').readline()==b'blocked\n'; peer.close()
now(2)
print('AUTHORITY_VERIFIED',flush=True)
"#;
    let other = r#"import socket,errno,time
probe=socket.socket(socket.AF_UNIX)
try: probe.connect('/run/seedbox/control.sock')
except OSError as error:
 assert error.errno==errno.ENOENT; print('NON_CONTROLLER_SOCKET_BLOCKED',error.errno,flush=True)
else: raise RuntimeError('second guest could access control socket')
probe.close()
deadline=time.monotonic()+30
while True:
 try: peer=socket.create_connection(('oracle',31415),timeout=1); break
 except OSError:
  assert time.monotonic()<deadline; time.sleep(0.001)
peer.sendall(b'blocked\n'); peer.close()
"#;
    let project = serde_json::json!({"name":"two-guest-authority","machines":{"other":{"image":"docker.io/library/python:3.13-alpine","command":["python3","-u","-c",other]}},"oracle":{"image":"docker.io/library/python:3.13-alpine","command":["python3","-u","-c",oracle]},"network":{"latency":"1ms"}});
    let path = service.path().join("seedbox.yaml");
    fs::write(&path, serde_json::to_vec(&project).unwrap()).unwrap();
    let id = service.run(&path);
    let other = service.cli(&["logs", &id, "other"]);
    assert!(String::from_utf8_lossy(&other.stdout).contains("NON_CONTROLLER_SOCKET_BLOCKED"));
    let oracle = service.cli(&["logs", &id, "oracle"]);
    let oracle = String::from_utf8_lossy(&oracle.stdout);
    assert_eq!(oracle.matches("CONTROLLER_ACK").count(), 2);
    assert!(oracle.contains("AUTHORITY_VERIFIED"));
    eprintln!("two-guest native authority: non-controller connect ENOENT; controller Now/Now acknowledged; oracle passed");
    service.replay(&id);
    service.close();
}

#[test]
fn normal_driver_tcg_explore_records_every_seed_backend() {
    let service = Service::new("tcg");
    let project = service.path().join("seedbox.yaml");
    fs::write(&project, serde_json::to_vec(&serde_json::json!({
        "name":"backend-range", "machines":{},
        "oracle":{"image":"docker.io/library/python:3.13-alpine", "command":["python3","-c","print('RANGE_BACKEND_OK')"]}
    })).unwrap()).unwrap();
    let output = service.cli(&[
        "run",
        project.to_str().unwrap(),
        "--backend",
        "tcg",
        "--seed",
        "123",
        "--count",
        "2",
        "--wall-time",
        "15m",
        "--json",
    ]);
    let range: seedbox_api::SeedRangeStatus = serde_json::from_slice(&output.stdout).unwrap();
    assert!(range.complete());
    assert!(range.error.is_none());
    assert_eq!(range.members.len(), 2);
    for member in &range.members {
        assert_eq!(member.status.oracle_exit_code, Some(0));
        service.assert_profile(member.metadata.id.as_str());
        assert_eq!(service.witness(member.metadata.id.as_str()).len(), 64);
    }
    eprintln!(
        "TCG explore retained backend for both seeds, first={}",
        range.first_seed
    );
    service.close();
}

fn copy_project(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_project(&entry.path(), &target.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}
