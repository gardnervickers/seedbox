//! Real CLI → ordinary OCI preparation → retained Driver → native PV acceptance.
//! Never substitutes a prepared manifest, executor, or execution report.
use seedbox_application::process_lifetime::{reclaim_barrier, EXECUTION_LEASE_FILE};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

struct Service {
    root: Option<tempfile::TempDir>,
    child: Option<Child>,
    binary: PathBuf,
    docker: std::ffi::OsString,
}
impl Service {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("efd-")
            .tempdir_in("/tmp")
            .unwrap();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
        let binary = target
            .join("release/seedbox")
            .canonicalize()
            .expect("normal cargo xtask build-assets and cargo build --release first");
        let docker = std::env::var_os("DOCKER_HOST")
            .unwrap_or_else(|| "unix:///run/podman/podman.sock".into());
        let mut this = Self {
            root: Some(root),
            child: None,
            binary,
            docker,
        };
        let log = fs::File::create(this.path().join("server.log")).unwrap();
        let child = this
            .command()
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        this.child = Some(child);
        while std::os::unix::net::UnixStream::connect(this.path().join("state/server.sock"))
            .is_err()
        {
            assert!(
                this.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "service exited: {}",
                fs::read_to_string(this.path().join("server.log")).unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        this
    }
    fn path(&self) -> &Path {
        self.root.as_ref().unwrap().path()
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env_clear()
            .env("HOME", self.path())
            .env("TMPDIR", self.path())
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env("SEEDBOX_STATE", self.path().join("state"))
            .env("DOCKER_HOST", &self.docker)
            .env("PATH", "")
            .current_dir(self.path());
        command
    }
    fn cli(&self, args: &[&str]) -> Output {
        let output = self.command().args(args).output().unwrap();
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
        let output = self.cli(&[
            "run",
            project.to_str().unwrap(),
            "--seed",
            "1009",
            "--wall-time",
            "15m",
            "--json",
        ]);
        let run: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let id = run["metadata"]["id"].as_str().unwrap().to_owned();
        self.assert_profile(&id);
        id
    }
    fn assert_profile(&self, id: &str) {
        let output = self.cli(&["describe", "simulation", id, "--json"]);
        let run: seedbox_api::Simulation = serde_json::from_slice(&output.stdout).unwrap();
        let seedbox_api::InputSource::Resolved { definition } = &run.spec.inputs else {
            panic!("completed ordinary run must retain a resolved definition")
        };
        let execution = &definition.runtime.execution;
        assert_eq!(
            execution["semantic_parameters"]["engine"]["engine_backend"], "driver-pv",
            "{execution}"
        );
        assert_eq!(execution["backend"], "driver-pv");
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
            let began = Instant::now();
            while child.try_wait().ok().flatten().is_none()
                && began.elapsed() < Duration::from_secs(30)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            if child.try_wait().ok().flatten().is_none() {
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
fn normal_driver_existing_example_run_and_replay_match() {
    let service = Service::new();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/redis")
        .canonicalize()
        .unwrap();
    let project = service.path().join("example");
    copy_project(&source, &project);
    let id = service.run(&project.join("compose.yaml"));
    fs::rename(&project, service.path().join("source-no-longer-available")).unwrap();
    service.replay(&id);
    service.close();
}

#[test]
fn normal_driver_two_guests_only_the_oracle_can_use_control() {
    let service = Service::new();
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
