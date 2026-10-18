//! Hardware acceptance of server-owned pause/resume using exact retained inputs.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use seedbox_api::{
    ComparisonStatus, ControlCommand, ControlRequest, ControlState, Phase, PlaybackRate,
    PlaybackStatus, Simulation, SimulationId, TestVerdict,
};
use seedbox_client::Client;
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// Private native-test probe of the existing host-wait transport, not a new
/// runtime API or a host elapsed-time oracle. host_clock documents its single
/// atomic word: odd means an open wait; a different odd token witnesses a new
/// end-wait / scheduler-attempt / begin-wait cycle. No timestamp is decoded.
struct PausedWaitProbe(NonNull<AtomicU64>);
impl PausedWaitProbe {
    fn open(clock: &Path) -> Self {
        let file = File::open(clock).unwrap();
        let size = std::mem::size_of::<AtomicU64>();
        assert_eq!(file.metadata().unwrap().len(), size as u64);
        // SAFETY: same one-word, process-shared atomic layout as host_clock;
        // its sole writer never truncates the file. Mapping is read-only,
        // page-aligned and kept alive until this probe's Drop.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        Self(NonNull::new(address.cast()).unwrap())
    }
    fn sample(&self) -> u64 {
        // SAFETY: the validated mmap contains one initialized AtomicU64; all
        // accesses after creation (including the real worker's) are atomic.
        unsafe { self.0.as_ref() }.load(Ordering::Acquire)
    }
    fn new_open_wait(&self, previous: u64) -> Option<()> {
        let word = self.sample();
        (word & 1 != 0 && word != previous).then_some(())
    }
}
impl Drop for PausedWaitProbe {
    fn drop(&mut self) {
        // SAFETY: release this probe's exact mapping, not the worker's file.
        unsafe {
            libc::munmap(self.0.as_ptr().cast(), std::mem::size_of::<AtomicU64>());
        }
    }
}
fn paused_wait_probe(state: &Path) -> PausedWaitProbe {
    // Follow only live endpoint locators beneath THIS fixture's state root.
    // Completed executions remove their locator, so the sole live paused run
    // supplies the clock. Never scan unrelated /tmp directories or PIDs.
    let mut pending = vec![state.to_owned()];
    let mut clocks: Vec<PathBuf> = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else if entry.file_name() == "host-control" {
                let socket = PathBuf::from(fs::read_to_string(entry.path()).unwrap());
                let clock = socket.with_file_name(seedbox_engine_protocol::host_clock::FILE_NAME);
                if socket.exists() && clock.exists() {
                    clocks.push(clock);
                }
            }
        }
    }
    assert_eq!(
        clocks.len(),
        1,
        "fixture must own exactly one live paused worker: {clocks:?}"
    );
    PausedWaitProbe::open(&clocks[0])
}

#[test]
fn pause_wait_probe_requires_a_new_open_wait_signal() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("clock");
    let _owner = seedbox_engine_protocol::host_clock::Reader::create(&path).unwrap();
    let mut writer = seedbox_engine_protocol::host_clock::Writer::open(&path).unwrap();
    let probe = PausedWaitProbe::open(&path);
    let closed = probe.sample();
    assert!(probe.new_open_wait(closed).is_none());
    writer.begin_wait().unwrap();
    assert!(probe.new_open_wait(closed).is_some());
    let open = probe.sample();
    assert!(
        probe.new_open_wait(open).is_none(),
        "the old open wait is not a new scheduler witness"
    );
    writer.end_wait().unwrap();
    assert!(
        probe.new_open_wait(open).is_none(),
        "a closed wait is not a paused scheduling witness"
    );
}

fn cli(binary: &Path, state: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_remove("SEEDBOX_VMM")
        .env("SEEDBOX_STATE", state);
    command
}

fn simulation(output: Output) -> Simulation {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn set_rate(client: &Client, id: &SimulationId, rate: PlaybackRate) -> PlaybackStatus {
    let request = ControlRequest {
        request_id: Client::request_id().unwrap(),
        command: ControlCommand::SetRate { rate },
    };
    let mut receipt = client.request_control(id, &request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match receipt.state {
                ControlState::Acknowledged {
                    status,
                    error: None,
                    ..
                } => {
                    assert_eq!(status.rate, rate);
                    return status;
                }
                ControlState::Accepted => {}
                state => panic!("playback command failed: {state:?}"),
            }
            receipt = client
                .control_receipt(id, &request.request_id)
                .await
                .unwrap();
        }
    })
    .await
    .expect("playback command acknowledgement")
}

#[test]
fn paused_start_and_resume_has_identical_witness_and_full_state() {
    let binary = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    // Unix socket paths must fit even when the checkout/report path is long.
    let scratch = tempfile::Builder::new()
        .prefix("sb-pause-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = scratch.path().join("state");
    let binary = Path::new(&binary);
    let _server = Server::start(binary, &state);
    let workload = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../harness/acceptance/workloads/key-value.yaml");
    let source = simulation(
        cli(binary, &state)
            .arg("run")
            .arg(&workload)
            .args(["--seed", "1009", "--json"])
            .guarded_output()
            .unwrap(),
    );
    assert_eq!(source.status.verdict(), TestVerdict::Passed);
    let rerun = simulation(
        cli(binary, &state)
            .arg("run")
            .arg(&workload)
            .args(["--seed", "1009", "--paused", "--detach", "--json"])
            .guarded_output()
            .unwrap(),
    );
    assert!(
        rerun.spec.program.is_empty(),
        "paused-gate witness requires this fixture's empty authored-input program"
    );
    let client = Client::connect_to(&state.join("server.sock")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(900), async {
            let id = &rerun.metadata.id;
            loop {
                match client.playback(id).await {
                    Ok(observation) => {
                        assert!(!observation.state.terminal, "run ended before resume");
                        assert_eq!(observation.state.rate, PlaybackRate::Paused);
                        assert_eq!(observation.state.position.completed_rounds, 0);
                        assert_eq!(observation.state.position.simulated_time_ns, 0);
                        break;
                    }
                    Err(seedbox_client::Error::Rejected {
                        code: seedbox_api::ErrorCode::Unavailable,
                        ..
                    }) => {}
                    Err(error) => panic!("playback unavailable: {error}"),
                }
                let current = client.describe(id).await.unwrap();
                assert_ne!(current.status.phase, Phase::Completed, "{current:?}");
            }
            let paused = set_rate(&client, id, PlaybackRate::Paused).await;
            let probe = paused_wait_probe(&state);
            let previous = probe.sample();
            // Correlated barrier work gives even a blocking-control implementation
            // a new scheduling opportunity. With a start-paused world at round 0,
            // no authored input/held fault/prepared request exists: the only open
            // wait here follows Driver::step returning Waiting for the paused gate.
            // Observe a NEW wait cycle after the first ACK, not a timed absence or
            // an arbitrary number of repeated HTTP position reads.
            let confirmed = set_rate(&client, id, PlaybackRate::Paused).await;
            assert_eq!(confirmed.position, paused.position);
            HangGuard::new(
                "native paused scheduler must signal a fresh declined-step wait",
                Duration::from_secs(30),
            )
            .wait_for(|| probe.new_open_wait(previous))
            .unwrap();
            // Read the position through a fresh correlated worker barrier AFTER
            // the signal, not merely the server's possibly lagging cached sample.
            let after_wait = set_rate(&client, id, PlaybackRate::Paused).await;
            assert_eq!(after_wait.position, paused.position);
            let observation = client.playback(id).await.unwrap();
            assert_eq!(observation.state.rate, PlaybackRate::Paused);
            assert_eq!(observation.state.position, paused.position);
            assert_ne!(
                client.describe(id).await.unwrap().status.phase,
                Phase::Completed
            );
            set_rate(&client, id, PlaybackRate::Unlimited).await;

            loop {
                let current = client.describe(id).await.unwrap();
                if current.status.phase == Phase::Completed {
                    assert_eq!(current.status.verdict(), TestVerdict::Passed, "{current:?}");
                    break;
                }
            }
            let comparison = client.compare(&source.metadata.id, id).await.unwrap();
            assert_eq!(comparison.status, ComparisonStatus::Match);
            assert!(comparison.outcome_matches);
            assert_eq!(comparison.witness_matches, Some(true));
            assert_eq!(comparison.full_state_matches, Some(true));
        })
        .await
        .expect("pause/resume acceptance completes");
    });
}

/// The fixture stops its server before its private state directory is removed.
struct Server(Child);
impl Server {
    fn start(binary: &Path, state: &Path) -> Self {
        let mut server = Self(
            cli(binary, state)
                .args([
                    "server",
                    "start",
                    "--foreground",
                    "--startup-handshake",
                    "--full-state",
                ])
                .stdout(Stdio::piped())
                .guarded_spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        BufReader::new(server.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready["status"], "ready");
        server
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: this is the unreaped child owned by this fixture.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let pid = self.0.id();
        std::thread::scope(|scope| {
            let (done, completed) = std::sync::mpsc::channel();
            scope.spawn(move || {
                // Observe exit without reaping: the PID stays owned until the
                // main thread has decided whether it must send SIGKILL.
                let result = loop {
                    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
                    // SAFETY: info is writable and pid names our unreaped child.
                    let rc = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            pid,
                            info.as_mut_ptr(),
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if rc == 0 {
                        break Ok(());
                    }
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        break Err(error);
                    }
                };
                let _ = done.send(result);
            });
            if !matches!(
                completed.recv_timeout(std::time::Duration::from_secs(30)),
                Ok(Ok(()))
            ) {
                let _ = self.0.kill();
            }
        });
        self.0.wait().expect("reap owned pause-test server");
    }
}
