//! Real host workers with an explicit fake protocol, never KVM/HVF or guests.
#![cfg(all(unix, feature = "test-support"))]
use seedbox_machine::{
    definition::DiskImage,
    execution::ExecutionConfig,
    machine::{Machine, RunContext, Timestamp},
    pv::{Inbox, Input, Outbox, Request},
    rng::RngStream,
    test_support::{assets, factory_definition, FixedClock},
};
use seedbox_machine_pv::machine::pv::{Factory, PVMachine, Snapshot};
use seedbox_test::process::Guarded;
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
const CHILD: &str = env!("CARGO_BIN_EXE_seedbox-pv-scratch-fixture");
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn wrapper(root: &Path, mode: &str) -> (PathBuf, PathBuf) {
    let record = root.join(format!("{mode}-records.ndjson"));
    let executable = root.join(format!("{mode}-exec-wrapper"));
    // Written and closed while forks are excluded: another thread of this
    // test binary (or the library's own worker spawn) can't copy a writable
    // descriptor and make the wrapper's exec fail with ETXTBSY.
    seedbox_test::process::install_executable(
        &executable,
        format!(
            "#!/bin/sh\nexec {} --scratch-fixture --scratch-mode {} --scratch-record {} \"$@\"\n",
            quote(CHILD),
            quote(mode),
            quote(record.to_str().unwrap())
        )
        .as_bytes(),
        0o700,
    )
    .unwrap();
    (executable, record)
}
fn events(record: &Path) -> Vec<Value> {
    fs::read_to_string(record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn reaped(record: &Path) {
    // Only Linux exposes reaping through /proc; macOS checks nothing here.
    #[cfg(not(target_os = "linux"))]
    let _ = record;
    #[cfg(target_os = "linux")]
    for event in events(record).iter().filter(|v| v["kind"] == "start") {
        assert!(
            !Path::new("/proc")
                .join(event["pid"].as_u64().unwrap().to_string())
                .exists(),
            "fixture worker was not reaped"
        );
    }
}
fn root(parent: &Path) -> PathBuf {
    let root = parent
        .join("session,offset=4096 ' \" \\")
        .join("long-".repeat(25))
        .join("deep-".repeat(25));
    fs::create_dir_all(&root).unwrap();
    assert!(root.as_os_str().len() > 108);
    root
}
fn owned_dirs(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("seedbox-pv-")
        })
        .collect()
}
fn transients(root: &Path) -> usize {
    fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().path())
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            usize::from(
                name.starts_with("capture-")
                    || name.starts_with("disk-export-")
                    || name.starts_with(".tmp"),
            ) + if p.is_dir() { transients(&p) } else { 0 }
        })
        .sum()
}
/// Genuine cold Factory output using structural ELF/kernel header data. No
/// fixture program is assembled/executed; all boot goes to the fake worker.
fn initial() -> Snapshot {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(target_arch = "x86_64")]
    let (machine, loader, kernel) = {
        // Structural boot-placement fields, not executable fixture code.
        let mut kernel = assets::bzimage_header();
        kernel[0x236..0x238].copy_from_slice(&0x21u16.to_le_bytes());
        kernel[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
        kernel[0x258..0x260].copy_from_slice(&0x1000000u64.to_le_bytes());
        kernel[0x260..0x264].copy_from_slice(&(16u32 << 20).to_le_bytes());
        (62, "lib/ld-linux-x86-64.so.2", kernel)
    };
    #[cfg(target_arch = "aarch64")]
    let (machine, loader, kernel) = {
        let mut kernel = vec![0; 4096];
        kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
        kernel[56..60].copy_from_slice(b"ARM\x64");
        (183, "lib/ld-linux-aarch64.so.1", kernel)
    };
    fs::write(directory.path().join("kernel"), kernel).unwrap();
    fs::write(directory.path().join("init"), assets::image(machine)).unwrap();
    fs::write(directory.path().join("command-line"), "rdinit=/init").unwrap();
    let mut archive = tar::Builder::new(Vec::new());
    for name in [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        loader,
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ] {
        let bytes = if name.ends_with(".txt") || name.ends_with(".LIB") {
            b"license".to_vec()
        } else {
            assets::image(machine)
        };
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o755);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(bytes.len() as u64);
        h.set_cksum();
        archive
            .append_data(&mut h, format!("seedbox/progress/{name}"), &bytes[..])
            .unwrap();
    }
    fs::write(
        directory.path().join("progress.tar"),
        archive.into_inner().unwrap(),
    )
    .unwrap();
    let mut factory = Factory::new(Path::new(CHILD), directory.path(), 0).unwrap();
    let mut definition = factory_definition::definition(0);
    definition.disk = Some(DiskImage::from_bytes(vec![9; 2048]).unwrap());
    let mut machine = factory.create(&definition).unwrap();
    machine.snapshot().unwrap()
}
#[derive(Default)]
struct Entropy(Vec<usize>);
impl RngStream for Entropy {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        self.0.push(bytes.len());
        bytes.fill(self.0.len() as u8);
    }
}
fn run(m: &mut PVMachine, rng: &mut Entropy, inputs: Vec<Request>) -> Result<(), std::io::Error> {
    let now = Timestamp::from_nanos(0);
    let mut inputs: VecDeque<_> = inputs
        .into_iter()
        .map(|payload| Input { at: now, payload })
        .collect();
    let mut outputs = Vec::new();
    let mut context = RunContext {
        worker: &mut (),
        clock: &FixedClock(0),
        rng,
        execution: ExecutionConfig::default(),
        inbox: Inbox::new(&mut inputs, now),
        outbox: Outbox::new(&mut outputs, now, now),
    };
    m.run(&mut context, now)?;
    assert!(inputs.is_empty());
    assert!(outputs.is_empty());
    Ok(())
}

#[test]
fn abrupt_cold_owner_death_leaves_only_session_reclaimable_files() {
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let target = tempfile::tempdir().unwrap();
    let root = root(target.path());
    let artifact = root.join("caller-snapshot");
    initial()
        .write_to(&mut fs::File::create(&artifact).unwrap())
        .unwrap();
    let marker = root.join("caller-ready");
    let mut owner = Owned(
        Command::new(CHILD)
            .args(["--scratch-fixture", "--scratch-owner-probe"])
            .arg("--scratch-root")
            .arg(&root)
            .arg("--scratch-snapshot")
            .arg(artifact)
            .arg("--scratch-ready")
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .guarded_spawn()
            .unwrap(),
    );
    seedbox_test_time::HangGuard::new(
        "cold scratch owner publishes its actual ready marker",
        std::time::Duration::from_secs(5),
    )
    .wait_for(|| {
        if marker.exists() {
            return Some(());
        }
        assert!(
            owner.0.try_wait().unwrap().is_none(),
            "cold scratch owner exited before ready"
        );
        None
    })
    .expect("cold scratch owner did not become ready");
    let owned = owned_dirs(&root);
    assert_eq!(owned.len(), 1);
    owner.0.kill().unwrap(); // Actual abrupt death: no Rust destructor can run.
    owner.0.wait().unwrap();
    assert!(owned[0].is_dir());
    assert!(root.is_dir(), "backend never removes the caller's root");
    // Explicit caller/session reclamation, not an actual server startup test.
    fs::remove_dir_all(&root).unwrap();
    assert!(!owned[0].exists());
    assert!(target.path().is_dir());
}

#[test]
fn non_utf8_absolute_and_effective_relative_or_default_roots_fail_without_side_effects() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let sandbox = tempfile::tempdir().unwrap();
    let bad = sandbox
        .path()
        .join(OsString::from_vec(b"caller-\xff".to_vec()));
    fs::create_dir(&bad).unwrap();
    let relative = bad.join("relative");
    fs::create_dir(&relative).unwrap();
    for root in [&bad, &relative] {
        fs::write(root.join("caller-marker"), b"keep exact bytes").unwrap();
    }
    let artifact = sandbox.path().join("cold-snapshot");
    initial()
        .write_to(&mut fs::File::create(&artifact).unwrap())
        .unwrap();
    for case in 0..4 {
        let result = sandbox.path().join(format!("error-{case}"));
        let mut command = Command::new(CHILD);
        command
            .args(["--scratch-fixture", "--scratch-encoding-probe"])
            .arg("--scratch-snapshot")
            .arg(&artifact)
            .arg("--scratch-result")
            .arg(&result);
        match case {
            0 => {
                command.arg("--scratch-root").arg(&bad);
            }
            1 => {
                command
                    .current_dir(&bad)
                    .arg("--scratch-root")
                    .arg("relative");
            }
            2 => {
                command.env("TMPDIR", &bad);
            }
            3 => {
                command.current_dir(&bad).env("TMPDIR", "relative");
            }
            _ => unreachable!(),
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .guarded_status()
            .unwrap();
        assert!(status.success(), "encoding probe failed, case {case}");
        let message = fs::read_to_string(result).unwrap();
        assert!(
            message.contains("UTF-8") && message.contains("choose"),
            "{message}"
        );
        assert_eq!(
            fs::read_dir(&bad).unwrap().count(),
            2,
            "partial directory in raw-byte root, case {case}"
        );
        assert_eq!(
            fs::read_dir(&relative).unwrap().count(),
            1,
            "partial directory in effective relative root, case {case}"
        );
        for root in [&bad, &relative] {
            assert_eq!(
                fs::read(root.join("caller-marker")).unwrap(),
                b"keep exact bytes"
            );
        }
    }
}

#[test]
fn all_valid_utf8_absolute_relative_and_default_spellings_remain_supported() {
    let sandbox = tempfile::tempdir().unwrap();
    let valid = sandbox
        .path()
        .join("UTF-8:α,offset=12 ' \" \\")
        .join("long-".repeat(25));
    fs::create_dir_all(&valid).unwrap();
    let relative = valid.join("relative");
    fs::create_dir(&relative).unwrap();
    for root in [&valid, &relative] {
        fs::write(root.join("caller-marker"), b"keep").unwrap();
    }
    let artifact = sandbox.path().join("cold-snapshot");
    initial()
        .write_to(&mut fs::File::create(&artifact).unwrap())
        .unwrap();
    for case in 0..4 {
        let result = sandbox.path().join(format!("ok-{case}"));
        let mut command = Command::new(CHILD);
        command
            .args([
                "--scratch-fixture",
                "--scratch-encoding-probe",
                "--scratch-expect-ok",
            ])
            .arg("--scratch-snapshot")
            .arg(&artifact)
            .arg("--scratch-result")
            .arg(&result);
        match case {
            0 => {
                command.arg("--scratch-root").arg(&valid);
            }
            1 => {
                command
                    .current_dir(&valid)
                    .arg("--scratch-root")
                    .arg("relative");
            }
            2 => {
                command.env("TMPDIR", &valid);
            }
            3 => {
                command.current_dir(&valid).env("TMPDIR", "relative");
            }
            _ => unreachable!(),
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .guarded_status()
            .unwrap();
        assert!(status.success(), "valid path probe failed, case {case}");
        assert_eq!(fs::read_to_string(result).unwrap(), "ok");
        assert_eq!(fs::read_dir(&valid).unwrap().count(), 2);
        assert_eq!(fs::read_dir(&relative).unwrap().count(), 1);
        for root in [&valid, &relative] {
            assert_eq!(fs::read(root.join("caller-marker")).unwrap(), b"keep");
        }
    }
}

#[test]
fn tempfile_override_is_default_authority_but_explicit_root_still_wins() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let sandbox = tempfile::tempdir().unwrap();
    let valid = sandbox.path().join("override");
    let environment = sandbox.path().join("environment");
    let explicit = sandbox.path().join("explicit");
    let bad = sandbox
        .path()
        .join(OsString::from_vec(b"override-\xff".to_vec()));
    let relative = bad.join("relative");
    let roots = [&valid, &environment, &explicit, &bad, &relative];
    for root in roots {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("caller-marker"), b"keep authority").unwrap();
    }
    let artifact = sandbox.path().join("cold-snapshot");
    initial()
        .write_to(&mut fs::File::create(&artifact).unwrap())
        .unwrap();
    for case in 0..5 {
        let (override_root, temp_root, explicit_root, expect_ok) = match case {
            0 => (valid.as_path(), bad.as_path(), None, true),
            1 => (bad.as_path(), environment.as_path(), None, false),
            2 => (bad.as_path(), bad.as_path(), Some(explicit.as_path()), true),
            3 => (
                valid.as_path(),
                environment.as_path(),
                Some(bad.as_path()),
                false,
            ),
            4 => (Path::new("relative"), environment.as_path(), None, false),
            _ => unreachable!(),
        };
        let result = sandbox.path().join(format!("override-result-{case}"));
        let mut command = Command::new(CHILD);
        command
            .args(["--scratch-fixture", "--scratch-encoding-probe"])
            .arg("--scratch-temp-override")
            .arg(override_root)
            .arg("--scratch-snapshot")
            .arg(&artifact)
            .arg("--scratch-result")
            .arg(&result)
            .env("TMPDIR", temp_root);
        if case == 4 {
            command.current_dir(&bad);
        }
        if let Some(root) = explicit_root {
            command.arg("--scratch-root").arg(root);
        }
        if expect_ok {
            command
                .arg("--scratch-expect-ok")
                .arg("--scratch-expected-parent")
                .arg(if case == 0 { &valid } else { &explicit });
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .guarded_status()
            .unwrap();
        assert!(
            status.success(),
            "tempfile authority probe failed, case {case}"
        );
        let message = fs::read_to_string(result).unwrap();
        if expect_ok {
            assert_eq!(message, "ok");
        } else {
            assert!(
                message.contains("UTF-8") && message.contains("choose"),
                "{message}"
            );
        }
        for root in roots {
            assert_eq!(
                fs::read_dir(root).unwrap().count(),
                if root == &bad { 2 } else { 1 },
                "wrong/partial authority, case {case}: {root:?}"
            );
            assert_eq!(
                fs::read(root.join("caller-marker")).unwrap(),
                b"keep authority"
            );
        }
    }
}

#[test]
fn fixture_without_explicit_mode_is_benign() {
    for args in [vec![], vec!["--help"], vec!["--test-threads=1"]] {
        let result = Command::new(CHILD)
            .args(args)
            .stdin(Stdio::null())
            .guarded_output()
            .unwrap();
        assert!(result.status.success());
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }
}
#[test]
fn exec_wrapper_fresh_boot_reboots_recaptures_and_restores_under_new_authority() {
    let source = tempfile::tempdir().unwrap();
    let root = root(source.path());
    let (binary, record) = wrapper(&root, "ok");
    let mut m = PVMachine::from_snapshot_in(&binary, &initial(), &root).unwrap();
    let mut rng = Entropy::default();
    run(&mut m, &mut rng, vec![]).unwrap();
    let owned = owned_dirs(&root);
    assert_eq!(owned.len(), 1);
    for name in ["kernel", "initrd", "disk-base", "disk-overlay"] {
        assert!(owned[0].join(name).is_file());
    }
    run(&mut m, &mut rng, vec![Request::Reboot, Request::Reboot]).unwrap();
    assert_eq!(rng.0, [8, 8, 8]);
    let mut saved = None;
    for _ in 0..2 {
        saved = Some(m.snapshot().unwrap());
        assert_eq!(transients(&owned[0]), 0);
    }
    assert_eq!(
        events(&record)
            .iter()
            .filter(|v| v["kind"] == "start")
            .count(),
        3
    );
    drop(m);
    assert!(!owned[0].exists());
    reaped(&record);
    source.close().unwrap();
    let saved = saved.unwrap();
    let target = tempfile::tempdir().unwrap();
    let new_root = self::root(target.path());
    let (binary, record) = wrapper(&new_root, "ok");
    for _ in 0..2 {
        let mut restored = PVMachine::from_snapshot_in(&binary, &saved, &new_root).unwrap();
        let owned = owned_dirs(&new_root);
        assert_eq!(owned.len(), 1);
        assert_eq!(
            transients(&owned[0]),
            0,
            "restore image retained after Ready"
        );
        restored.snapshot().unwrap();
        assert_eq!(transients(&owned[0]), 0);
        drop(restored);
        assert!(owned_dirs(&new_root).is_empty());
    }
    for event in events(&record) {
        if let Some(path) = event["command"]["Boot"]["restore"].as_str() {
            assert!(Path::new(path).starts_with(&new_root));
            assert!(!Path::new(path).exists());
        }
    }
    reaped(&record);
    assert!(new_root.exists());
}
#[test]
fn restore_and_capture_and_export_errors_remove_partial_bodies_then_reap() {
    let source = tempfile::tempdir().unwrap();
    let source_root = root(source.path());
    let (binary, record) = wrapper(&source_root, "ok");
    let mut m = PVMachine::from_snapshot_in(&binary, &initial(), &source_root).unwrap();
    run(&mut m, &mut Entropy::default(), vec![]).unwrap();
    let saved = m.snapshot().unwrap();
    drop(m);
    reaped(&record);
    for mode in ["boot-error", "capture-error", "export-error"] {
        let target = tempfile::tempdir().unwrap();
        let root = root(target.path());
        let (binary, record) = wrapper(&root, mode);
        if mode == "boot-error" {
            assert!(PVMachine::from_snapshot_in(&binary, &saved, &root).is_err());
        } else {
            let mut m = PVMachine::from_snapshot_in(&binary, &saved, &root).unwrap();
            if mode == "capture-error" {
                assert!(m.snapshot().is_err());
            } else {
                assert!(run(&mut m, &mut Entropy::default(), vec![Request::Crash]).is_err());
            }
            assert_eq!(transients(&owned_dirs(&root)[0]), 0);
            // No run/retry after an error; only teardown follows.
            drop(m);
        }
        assert!(owned_dirs(&root).is_empty());
        assert!(root.is_dir());
        reaped(&record);
    }
}

/// bd-7bd, deterministically: a fork held before exec while the wrapper is
/// still open for writing in this process makes the PV worker launch fail;
/// installed wrappers launch under concurrent held forks. Linux ETXTBSY only.
#[cfg(target_os = "linux")]
mod descriptor_probe {
    use super::*;
    use seedbox_test::process::{install_executable, HeldChildren, Spawn};
    use std::io::Write;

    /// A cold snapshot starts the worker on its first run.
    fn launch(binary: &Path, saved: &Snapshot, root: &Path) -> Result<PVMachine, String> {
        let mut m = PVMachine::from_snapshot_in(binary, saved, root).map_err(|e| e.to_string())?;
        run(&mut m, &mut Entropy::default(), vec![]).map_err(|e| e.to_string())?;
        Ok(m)
    }
    fn script(record: &Path) -> String {
        format!(
            "#!/bin/sh\nexec {} --scratch-fixture --scratch-mode ok --scratch-record {} \"$@\"\n",
            quote(CHILD),
            quote(record.to_str().unwrap())
        )
    }

    #[test]
    fn wrapper_written_in_process_cannot_start_while_a_fork_is_held() {
        let saved = initial();
        let source = tempfile::tempdir().unwrap();
        let root = root(source.path());
        let binary = root.join("ok-exec-wrapper");
        // The helper's previous shape: an ordinary in-process write.
        let mut file = fs::File::create(&binary).unwrap();
        file.write_all(script(&root.join("records")).as_bytes())
            .unwrap();
        let mut held = HeldChildren::new();
        held.hold(Spawn::GuardForkOnly).unwrap();
        drop(file);
        install_mode(&binary);
        let error = match launch(&binary, &saved, &root) {
            Ok(_) => panic!("launched while a held fork had a writable copy"),
            Err(error) => error,
        };
        assert!(error.contains("os error 26"), "{error}");
        held.release().unwrap();
    }

    #[test]
    fn installed_wrapper_launches_while_forks_are_held_before_exec() {
        let saved = initial();
        let source = tempfile::tempdir().unwrap();
        let root = root(source.path());
        let record = root.join("ok-records.ndjson");
        let binary = root.join("ok-exec-wrapper");
        // Children forked before and after the install stay held through the
        // launch. Fork-only guarding keeps sibling tests' installs and this
        // launch free to proceed (a child held with the guard would make the
        // launch wait behind any sibling's queued install).
        let mut held = HeldChildren::new();
        held.hold(Spawn::GuardForkOnly).unwrap();
        install_executable(&binary, script(&record).as_bytes(), 0o700).unwrap();
        held.hold(Spawn::GuardForkOnly).unwrap();
        let m = launch(&binary, &saved, &root).unwrap();
        held.release().unwrap();
        drop(m);
        reaped(&record);
    }

    fn install_mode(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
