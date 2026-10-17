//! Process-global signal/subreaper mutations run only in disposable subprocesses.
use super::{
    options::Options,
    supervisor::{self, OwnedGroup, Trace},
};
use seedbox_test_time::HangGuard;
use std::{
    ffi::OsString,
    fs::{self, File},
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

fn options(root: &Path, seeds: &[i32], timeout: f64) -> Options {
    Options {
        root: root.into(),
        world: "world.json".into(),
        prefix: "control".into(),
        seeds: seeds.iter().map(ToString::to_string).collect(),
        extra: vec!["--digest".into()],
        timeout,
        vmm: root.join("worker").into_os_string(),
    }
}
fn output(root: &Path, seed: i32, index: usize) -> PathBuf {
    root.join(format!("runs/control-s{seed}-r{index}"))
}
fn suffix(path: &Path, extension: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(extension);
    path.into()
}
fn worker(root: &Path, body: &str) {
    fs::write(
        root.join("worker"),
        format!("#!/bin/sh\nset -eu\nout=$6\nprintf '%s\\n' \"$$\" > \"${{out}}.pid\"\n{body}\n"),
    )
    .unwrap();
    fs::set_permissions(root.join("worker"), fs::Permissions::from_mode(0o755)).unwrap();
}
fn gone(pid: i32) {
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "pid {pid} still exists");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
fn groups_gone(trace: &[Trace]) {
    for event in trace {
        if let Trace::Spawn { pid, .. } = event {
            gone(*pid);
            gone(-pid);
        }
    }
}
fn wait_until(mut predicate: impl FnMut() -> bool) {
    HangGuard::new("positive fixture readiness", Duration::from_secs(60))
        .wait_for(|| predicate().then_some(()))
        .expect("fixture readiness infrastructure hang");
}

// No autonomous worker deadline may race the parent's readiness/signal.
const BLOCK_UNTIL_SIGNAL: &str =
    "mkfifo \"${out}.blocked\"\nprintf ready > \"${out}.ready\"\nexec cat \"${out}.blocked\"";

struct Disposable(Child);
impl Drop for Disposable {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
            // This is only an unwind cleanup grace period. Expiry still falls
            // through to forced kill/reap; it never supplies an application result.
            let _ = HangGuard::new("disposable supervisor TERM cleanup", Duration::from_secs(5))
                .wait_for(|| self.0.try_wait().ok().flatten());
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn case(name: &str) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let stdout = root.join("supervisor.stdout");
    let stderr = root.join("supervisor.stderr");
    let mut child = Disposable(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::supervisor_case", "--nocapture"])
            .env("SEEDBOX_BATCH_TEST_CASE", name)
            .env("SEEDBOX_BATCH_TEST_ROOT", root)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    // Interrupt the supervisor only once every worker reports readiness. The
    // grace worker reports only after installing its TERM trap, so the group
    // TERM can never arrive before the trap.
    let ready: &[i32] = match name {
        "interrupt" | "interrupt-status-error" => &[0, 1],
        "grace" => &[0],
        _ => &[],
    };
    if !ready.is_empty() {
        wait_until(|| {
            ready
                .iter()
                .all(|i| suffix(&output(root, *i, *i as usize), ".ready").exists())
        });
        assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    }
    let status = HangGuard::new("supervisor subprocess must exit", Duration::from_secs(60))
        .wait_for(|| child.0.try_wait().unwrap())
        .unwrap_or_else(|error| {
            panic!(
                "{name}: subprocess infrastructure hang: {error}\n{}\n{}",
                fs::read_to_string(&stdout).unwrap(),
                fs::read_to_string(&stderr).unwrap()
            )
        });
    assert!(
        status.success(),
        "{name}: {status}\n{}\n{}",
        fs::read_to_string(&stdout).unwrap(),
        fs::read_to_string(&stderr).unwrap()
    );
    let child_stdout = fs::read_to_string(&stdout).unwrap();
    assert!(
        child_stdout.contains("1 passed; 0 failed"),
        "supervisor case {name} did not execute: {child_stdout}"
    );
    let diagnostics = fs::read_to_string(stderr).unwrap();
    if name.contains("status-error") {
        assert!(
            diagnostics.contains(".status"),
            "status failure must name its path: {diagnostics}"
        );
    }
    if name == "spawn-error" {
        assert!(
            diagnostics.contains(root.join("worker").to_str().unwrap()),
            "launch failure must name its command: {diagnostics}"
        );
    }
}

#[test]
fn invalid_timeout_is_rejected_before_mutation() {
    case("invalid");
}
#[test]
fn batch_launches_all_workers_without_cpu_binding() {
    case("launch");
}
#[test]
fn timeout_reaps_every_owned_child() {
    case("timeout");
}
#[test]
fn interruption_reaps_every_concurrent_child() {
    case("interrupt");
}
#[test]
fn orphan_descendant_is_reaped_before_return() {
    case("orphan");
}
#[test]
fn collisions_never_overwrite_or_start_workers() {
    case("collision");
}
#[test]
fn status_write_failure_still_cleans_every_worker() {
    case("status-error");
}
#[test]
fn spawn_failure_reports_command_and_leaves_no_owned_process() {
    case("spawn-error");
}
#[test]
fn interruption_status_write_failure_still_cleans_later_groups() {
    case("interrupt-status-error");
}
#[test]
fn term_grace_then_kill_reaps_resistant_worker() {
    case("grace");
}
#[test]
fn signaled_worker_keeps_negative_child_status() {
    case("signaled");
}

// This is a private libtest entry, not a production root bypass. Running the
// ordinary suite calls it with no case and mutates no process-global state.
#[test]
fn supervisor_case() {
    let Ok(name) = std::env::var("SEEDBOX_BATCH_TEST_CASE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("SEEDBOX_BATCH_TEST_ROOT").unwrap());
    match name.as_str() {
        "invalid" => {
            let untouched = root.join("must-not-exist");
            let subreaper = || {
                let mut value: libc::c_int = -1;
                assert_eq!(
                    unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut value, 0, 0, 0) },
                    0
                );
                value
            };
            let before = subreaper();
            for timeout in [0.0, -1.0, f64::NAN, f64::INFINITY] {
                assert!(supervisor::run_batch(options(&untouched, &[1], timeout))
                    .unwrap_err()
                    .to_string()
                    .contains("nonempty seeds and positive timeout"));
                assert!(!untouched.exists());
                assert_eq!(subreaper(), before);
            }
            assert!(supervisor::run_batch(options(&untouched, &[], 1.0)).is_err());
            assert!(!untouched.exists());
            assert_eq!(subreaper(), before);
            assert!(supervisor::take_trace().is_empty());
        }
        "launch" => {
            worker(&root, "printf '%s\\n' \"$@\" > \"${out}.args\"\nprintf 'application bytes'\nprintf 'diagnostic bytes' >&2\nexit 0");
            let mut opts = options(&root, &[7, 8, 9], 1.0);
            opts.prefix = "batch".into();
            assert_eq!(supervisor::run_batch(opts).unwrap(), 0);
            let trace = supervisor::take_trace();
            assert!(
                matches!(trace.get(3), Some(Trace::Wait(_))),
                "all three launches precede first wait: {trace:?}"
            );
            let pids: Vec<_> = trace
                .iter()
                .filter_map(|e| match e {
                    Trace::Spawn { pid, .. } => Some(*pid),
                    _ => None,
                })
                .collect();
            for event in &trace {
                if let Trace::Wait(pid) = event {
                    assert!(pids.contains(pid));
                }
            }
            for (index, seed) in [7, 8, 9].iter().enumerate() {
                let Trace::Spawn {
                    pid,
                    session,
                    program,
                    args,
                } = &trace[index]
                else {
                    panic!("missing ordered launch");
                };
                assert_eq!(session, pid, "each worker starts its own session");
                assert_eq!(
                    program,
                    root.join("worker").as_os_str(),
                    "no taskset/cpu binding launcher"
                );
                let out = root.join(format!("runs/batch-s{seed}-r{index}"));
                let expected: Vec<OsString> = vec![
                    "--config".into(),
                    "world.json".into(),
                    "--seed".into(),
                    seed.to_string().into(),
                    "--out".into(),
                    out.as_os_str().to_owned(),
                    "--digest".into(),
                ];
                assert_eq!(args, &expected);
                let actual = fs::read_to_string(suffix(&out, ".args")).unwrap();
                assert_eq!(
                    actual,
                    expected
                        .iter()
                        .map(|s| format!("{}\n", s.to_str().unwrap()))
                        .collect::<String>()
                );
                assert_eq!(
                    fs::read(suffix(&out, ".json")).unwrap(),
                    b"application bytes"
                );
                assert_eq!(fs::read(suffix(&out, ".err")).unwrap(), b"diagnostic bytes");
                assert_eq!(fs::read_to_string(suffix(&out, ".status")).unwrap(), "0\n");
            }
            assert!(!fs::read_dir(&root).unwrap().any(|p| p
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "lock")));
            groups_gone(&trace);
        }
        "timeout" | "interrupt" | "grace" | "signaled" => {
            let body = match name.as_str() {
                "grace" => format!("trap '' TERM\n{BLOCK_UNTIL_SIGNAL}"),
                "interrupt" => BLOCK_UNTIL_SIGNAL.into(),
                "signaled" => "kill -TERM $$".into(),
                _ => "printf ready > \"${out}.ready\"\nexec sleep 60".into(),
            };
            worker(&root, &body);
            let seeds: &[i32] = if name == "interrupt" { &[0, 1] } else { &[0] };
            // Only the timeout subject may expire. All other cases must stop
            // through their selected signal, independently of scheduling delay.
            // f64::MAX is valid and exceeds every representable elapsed Duration;
            // the parent's HangGuard alone bounds an unresponsive test fixture.
            let timeout = if name == "timeout" { 0.08 } else { f64::MAX };
            let result = supervisor::run_batch(options(&root, seeds, timeout)).unwrap();
            let interrupted = matches!(name.as_str(), "interrupt" | "grace");
            assert_eq!(result, if interrupted { 143 } else { 0 });
            let expected = match name.as_str() {
                "interrupt" | "grace" => "143\n",
                "signaled" => "-15\n",
                _ => "124\n",
            };
            for (index, seed) in seeds.iter().enumerate() {
                assert_eq!(
                    fs::read_to_string(suffix(&output(&root, *seed, index), ".status")).unwrap(),
                    expected
                );
            }
            let trace = supervisor::take_trace();
            if name == "grace" {
                // A TERM-ignoring worker gets the whole grace: SIGTERM, then
                // the full two seconds of grace-clock polls with the leader
                // still running, then SIGKILL. Test grace time counts polls,
                // so this ordering is independent of host speed.
                let Some(Trace::Spawn { pid, .. }) = trace.first() else {
                    panic!("missing launch: {trace:?}");
                };
                let pid = *pid;
                let ours: Vec<_> = trace
                    .iter()
                    .filter(|event| match event {
                        Trace::Signal { pid: p, .. } | Trace::GraceExpired { pid: p, .. } => {
                            *p == pid
                        }
                        _ => false,
                    })
                    .collect();
                assert!(
                    matches!(
                        ours[..],
                        [
                            Trace::Signal { signal: libc::SIGTERM, .. },
                            Trace::GraceExpired { waited, .. },
                            Trace::Signal { signal: libc::SIGKILL, .. },
                        ] if *waited == Duration::from_secs(2)
                    ),
                    "{trace:?}"
                );
                // Every grace poll observed the leader still running.
                let term = trace
                    .iter()
                    .position(|e| {
                        matches!(
                            e,
                            Trace::Signal {
                                signal: libc::SIGTERM,
                                ..
                            }
                        )
                    })
                    .unwrap();
                let expired = trace
                    .iter()
                    .position(|e| matches!(e, Trace::GraceExpired { .. }))
                    .unwrap();
                let polls = trace[term..expired]
                    .iter()
                    .filter(|e| matches!(e, Trace::Wait(p) if *p == pid))
                    .count();
                assert_eq!(polls, 201, "{trace:?}");
            }
            assert_eq!(
                trace
                    .iter()
                    .filter(|e| matches!(e, Trace::Spawn { .. }))
                    .count(),
                seeds.len()
            );
            groups_gone(&trace);
        }
        "orphan" => {
            supervisor::enable_subreaper().unwrap();
            // A child in a different owned-by-test group must remain waitable;
            // waitpid(-owned_pgid), never waitpid(-1), may reap the orphan only.
            let mut unrelated = Command::new("/bin/sh")
                .args(["-c", "exit 23"])
                .spawn()
                .unwrap();
            // Make the unrelated child a reserved zombie before owned-group
            // cleanup, so a broad waitpid(-1) would steal its wait status.
            wait_until(|| {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe {
                        libc::waitid(
                            libc::P_PID,
                            unrelated.id(),
                            &mut info,
                            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                        )
                    },
                    0
                );
                (unsafe { info.si_pid() }) != 0
            });
            let pid_file = root.join("descendant");
            let script = "trap '' TERM\nsleep 60 &\nprintf '%s' \"$!\" > \"$1.tmp\"\nmv \"$1.tmp\" \"$1\"\nwait";
            let mut group = OwnedGroup::spawn(
                Command::new("/bin/sh")
                    .args(["-c", script, "fixture"])
                    .arg(&pid_file),
            )
            .unwrap();
            wait_until(|| pid_file.exists());
            let descendant: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
            assert_eq!(unsafe { libc::kill(group.pid(), libc::SIGKILL) }, 0);
            wait_until(|| group.exit_status().unwrap().is_some());
            // Repeated WNOWAIT observations leave the leader reserved.
            assert_eq!(group.exit_status().unwrap(), Some(-libc::SIGKILL));
            group.stop().unwrap();
            gone(descendant);
            gone(-group.pid());
            gone(group.pid());
            group.stop().unwrap(); // no new signal after a completed cleanup
            assert_eq!(unrelated.wait().unwrap().code(), Some(23));
        }
        "collision" => {
            for (index, extension) in ["", ".json", ".err", ".status", ".dangling"]
                .iter()
                .enumerate()
            {
                let mut opts = options(&root, &[7, 8, 9], 1.0);
                opts.prefix = format!("collision{index}");
                let final_out = root.join(format!("runs/collision{index}-s9-r2"));
                fs::create_dir_all(final_out.parent().unwrap()).unwrap();
                let occupied = suffix(
                    &final_out,
                    if *extension == ".dangling" {
                        ".json"
                    } else {
                        extension
                    },
                );
                if *extension == ".dangling" {
                    symlink("missing-target", &occupied).unwrap();
                } else {
                    fs::write(&occupied, b"sentinel").unwrap();
                }
                let error = supervisor::run_batch(opts).unwrap_err().to_string();
                assert!(error.contains("refusing to overwrite"), "{error}");
                if *extension != ".dangling" {
                    assert_eq!(fs::read(&occupied).unwrap(), b"sentinel");
                } else {
                    assert_eq!(
                        fs::read_link(&occupied).unwrap(),
                        Path::new("missing-target")
                    );
                }
                assert!(!root
                    .join(format!("runs/collision{index}-s7-r0.json"))
                    .exists());
                assert!(supervisor::take_trace().is_empty());
            }
        }
        "spawn-error" => {
            assert_eq!(supervisor::run_batch(options(&root, &[0], 1.0)).unwrap(), 1);
            assert!(supervisor::take_trace().is_empty());
            let out = output(&root, 0, 0);
            assert!(fs::read(suffix(&out, ".json")).unwrap().is_empty());
            assert!(fs::read(suffix(&out, ".err")).unwrap().is_empty());
            assert!(!suffix(&out, ".status").exists());
        }
        "status-error" | "interrupt-status-error" => {
            let interrupted = name == "interrupt-status-error";
            worker(
                &root,
                &if interrupted {
                    format!(
                        "if [ \"$4\" = 0 ]; then mkdir \"${{out}}.status\"; fi\n{BLOCK_UNTIL_SIGNAL}"
                    )
                } else {
                    "if [ \"$4\" = 0 ]; then mkdir \"${out}.status\"; exit 0; fi\nexec sleep 60"
                        .into()
                },
            );
            assert_eq!(
                supervisor::run_batch(options(&root, &[0, 1], f64::MAX)).unwrap(),
                if interrupted { 143 } else { 1 }
            );
            assert!(suffix(&output(&root, 0, 0), ".status").is_dir());
            assert_eq!(
                fs::read_to_string(suffix(&output(&root, 1, 1), ".status")).unwrap(),
                if interrupted { "143\n" } else { "1\n" }
            );
            let trace = supervisor::take_trace();
            assert_eq!(
                trace
                    .iter()
                    .filter(|e| matches!(e, Trace::Spawn { .. }))
                    .count(),
                2
            );
            groups_gone(&trace);
        }
        _ => panic!("unknown case {name}"),
    }
}

#[test]
fn wrapper_routes_to_prebuilt_rust_supervisor_without_executing_privilege_boundary() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/batch.sh");
    let text = fs::read_to_string(&path).unwrap();
    assert!(Command::new("bash")
        .arg("-n")
        .arg(path)
        .status()
        .unwrap()
        .success());
    assert!(text.find("cargo build").unwrap() < text.find("exec sudo").unwrap());
    assert!(text.contains("--bin seedbox-batch --release --locked --target-dir"));
    assert!(text.contains("for name in VMM SEEDBOX_TIMEOUT"));
    assert!(text.contains("forward=(SEEDBOX_ROOT=\"$PWD\")"));
    assert!(text.contains("run_root=$(cd \"${SEEDBOX_ROOT:-$checkout}\" && pwd)"));
    assert!(text.find("run_root=$(cd").unwrap() < text.find("cd \"$checkout\"").unwrap());
    assert!(text.find("cd \"$checkout\"").unwrap() < text.find("cargo build").unwrap());
    assert!(text.find("cargo build").unwrap() < text.find("cd \"$run_root\"").unwrap());
    assert!(text.contains(
        "exec sudo -n env \"${forward[@]}\" \"$checkout/target/release/seedbox-batch\" \"$@\""
    ));
    assert!(!text.contains("python"));
}
