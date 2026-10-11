#[cfg(target_os = "linux")]
#[path = "support/process.rs"]
mod process;
mod support;
#[cfg(target_os = "linux")]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn invalid_batch_timeouts_are_rejected_before_creating_outputs() {
    let directory = tempfile::tempdir().unwrap();
    for value in ["0", "-1", "float('nan')", "float('inf')"] {
        let output=support::python_api("harness/batch.py", &format!("subject.run_batch(Path(sys.argv[2]), 'world', 'control', [1], [], {value}, 'unused')"), &[directory.path()]);
        assert!(!output.status.success());
        assert!(!directory.path().join("runs").exists());
        assert!(String::from_utf8_lossy(&output.stderr).contains("positive timeout"));
    }
}

#[cfg(target_os = "linux")]
struct Batch(Child);
#[cfg(target_os = "linux")]
impl Drop for Batch {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
            self.0.wait().unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
fn start(root: &std::path::Path, mode: &str, timeout: f64, count: usize) -> Batch {
    let executable = root.join("worker");
    fs::write(
        &executable,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$SEEDBOX_BATCH_FIXTURE_ROOT/args-$$"
printf '%s\n' "$$" > "$SEEDBOX_BATCH_FIXTURE_ROOT/ready-$$"
if [ "$SEEDBOX_BATCH_FIXTURE_MODE" = orphan ]; then
    (trap '' TERM; exec sleep 60) &
    printf '%s\n' "$!" > "$SEEDBOX_BATCH_FIXTURE_ROOT/descendant-$$"
    exit 0
fi
if [ "$SEEDBOX_BATCH_FIXTURE_MODE" = success ]; then
    while [ ! -f "$SEEDBOX_BATCH_FIXTURE_ROOT/release" ]; do sleep .01; done
    exit 0
fi
sleep 60 &
wait
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let script = r#"
import importlib.util, sys
from pathlib import Path
spec=importlib.util.spec_from_file_location('batch',sys.argv[1])
batch=importlib.util.module_from_spec(spec)
spec.loader.exec_module(batch)
sys.exit(batch.run_batch(Path(sys.argv[2]),'world.json','batch',list(range(7,7+int(sys.argv[5]))),['--digest'],float(sys.argv[4]),sys.argv[3]))
"#;
    Batch(
        Command::new("python3")
            .args(["-c", script])
            .arg(support::root().join("harness/batch.py"))
            .arg(root)
            .arg(executable)
            .arg(timeout.to_string())
            .arg(count.to_string())
            .env("SEEDBOX_BATCH_FIXTURE_ROOT", root)
            .env("SEEDBOX_BATCH_FIXTURE_MODE", mode)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

#[cfg(target_os = "linux")]
fn files(root: &std::path::Path, prefix: &str) -> Vec<std::path::PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .map(|entry| entry.path())
        .collect()
}
#[cfg(target_os = "linux")]
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "batch fixture timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn batch_starts_all_workers_before_waiting_and_reaps_success_timeout_signal_and_orphan_groups() {
    if !process::isolated(concat!(module_path!(), "::batch_starts_all_workers_before_waiting_and_reaps_success_timeout_signal_and_orphan_groups")) {
        return;
    }
    // All processes, signals and the Linux subreaper belong to a child supervisor.
    // Other Rust tests can execute concurrently without inheriting its signal state.
    for (mode, count, timeout, interrupt, expected, status) in [
        ("success", 3, 10.0, false, 0, 0),
        ("timeout", 2, 0.25, false, 0, 124),
        ("interrupt", 2, 10.0, true, 143, 143),
        ("orphan", 1, 10.0, false, 0, 0),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut batch = start(root, mode, timeout, count);
        wait_for(|| files(root, "ready-").len() == count);
        if mode == "success" {
            assert!(batch.0.try_wait().unwrap().is_none());
            fs::write(root.join("release"), "").unwrap();
        }
        if interrupt {
            assert_eq!(unsafe { libc::kill(batch.0.id() as i32, libc::SIGTERM) }, 0);
        }
        let mut completed = None;
        wait_for(|| {
            completed = batch.0.try_wait().unwrap();
            completed.is_some()
        });
        assert_eq!(completed.unwrap().code(), Some(expected));
        for seed in 7..7 + count {
            let index = seed - 7;
            let out = root.join(format!("runs/batch-s{seed}-r{index}"));
            assert_eq!(
                fs::read_to_string(out.with_extension("status")).unwrap(),
                format!("{status}\n")
            );
            assert!(files(root, "args-")
                .iter()
                .any(|file| fs::read_to_string(file).unwrap()
                    == format!(
                        "--config\nworld.json\n--seed\n{seed}\n--out\n{}\n--digest\n",
                        out.display()
                    )));
        }
        for file in files(root, "ready-")
            .into_iter()
            .chain(files(root, "descendant-"))
        {
            let pid = fs::read_to_string(file)
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap();
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "owned process {pid} survived"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        assert!(files(root, ".").is_empty());
    }
}
