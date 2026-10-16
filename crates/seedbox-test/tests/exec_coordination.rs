//! The shared fork/exec coordination in seedbox_test::process, checked with
//! held-before-exec children: deterministic, no sleeps or retries.
#![cfg(target_os = "linux")]
use seedbox_test::process::{install_executable, output, spawn, status, HeldChildren, Spawn};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, process::Command};

const SCRIPT: &[u8] = b"#!/bin/sh\nexit 0\n";

/// Each test holds children across steps; tests in this binary must not
/// interleave, because the exec-file scope is process-wide.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn a_fork_during_an_uncoordinated_write_makes_the_file_busy() {
    let _serial = serial();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tool");
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(SCRIPT).unwrap();
    let mut held = HeldChildren::new();
    held.hold(Spawn::GuardForkOnly).unwrap();
    drop(file);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let error = status(&mut Command::new(&path)).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY), "{error}");
    held.release().unwrap();
    assert!(status(&mut Command::new(&path)).unwrap().success());
}

#[test]
fn installed_executables_run_while_guarded_forks_are_held() {
    let _serial = serial();
    let directory = tempfile::tempdir().unwrap();
    for round in 0..4 {
        let path = directory.path().join(format!("tool-{round}"));
        install_executable(&path, SCRIPT, 0o700).unwrap();
        let mut held = HeldChildren::new();
        for _ in 0..4 {
            held.hold(Spawn::GuardWhileHeld).unwrap();
        }
        // Guarded forks exclude writers; probe without blocking.
        assert!(seedbox_exec::try_writing().is_none());
        drop(held);
        assert!(status(&mut Command::new(&path)).unwrap().success());
        let mut command = Command::new(&path);
        assert!(output(&mut command).unwrap().status.success());
        assert!(spawn(&mut Command::new(&path))
            .unwrap()
            .wait()
            .unwrap()
            .success());
    }
    assert!(seedbox_exec::try_writing().is_some());
}

#[test]
fn many_held_children_release_without_joining_one_at_a_time() {
    // Later forks hold copies of earlier children's spawn status pipes; a
    // release-then-join per child could wait forever. Mixed modes too.
    let _serial = serial();
    for how in [Spawn::GuardForkOnly, Spawn::GuardWhileHeld] {
        for _ in 0..25 {
            let mut held = HeldChildren::new();
            for _ in 0..6 {
                held.hold(how).unwrap();
            }
            assert_eq!(held.len(), 6);
            held.release().unwrap();
        }
    }
}

#[test]
fn a_fork_only_hold_after_a_guarded_one_is_refused_not_deadlocked() {
    let _serial = serial();
    let mut held = HeldChildren::new();
    held.hold(Spawn::GuardWhileHeld).unwrap();
    let error = held.hold(Spawn::GuardForkOnly).unwrap_err();
    assert!(error.to_string().contains("would deadlock"), "{error}");
    held.release().unwrap();
}

#[test]
fn a_panicking_test_still_releases_its_held_children() {
    let _serial = serial();
    let result = std::panic::catch_unwind(|| {
        let mut held = HeldChildren::new();
        held.hold(Spawn::GuardForkOnly).unwrap();
        held.hold(Spawn::GuardForkOnly).unwrap();
        panic!("assertion failure while children are held");
    });
    assert!(result.is_err());
    // Drop released and reaped them: the guard is free again.
    assert!(seedbox_exec::try_writing().is_some());
}

#[test]
fn install_refuses_to_replace_an_existing_file() {
    let _serial = serial();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tool");
    install_executable(&path, SCRIPT, 0o700).unwrap();
    let error = install_executable(&path, SCRIPT, 0o700).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
}
