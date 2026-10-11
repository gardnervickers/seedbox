//! Admission itself uses disposable ordinary processes, without a hypervisor.
use seedbox_system_tests::KernelLease;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn lease_child() {
    let Some(directory) = std::env::var_os("SEEDBOX_LEASE_FIXTURE_DIRECTORY") else {
        return;
    };
    fs::write(
        std::env::var_os("SEEDBOX_LEASE_FIXTURE_WAITING").unwrap(),
        "waiting",
    )
    .unwrap();
    let _lease = KernelLease::acquire(Path::new(&directory)).unwrap();
    fs::write(
        std::env::var_os("SEEDBOX_LEASE_FIXTURE_ACQUIRED").unwrap(),
        "acquired",
    )
    .unwrap();
    let mut release = [0];
    std::io::stdin().read_exact(&mut release).unwrap();
}

struct OwnedChild(Child);
impl OwnedChild {
    fn release(mut self) {
        self.0.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(self.0.wait().unwrap().success());
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}
fn child(directory: &Path, acquired: &Path) -> OwnedChild {
    OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lease_child", "--nocapture"])
            .env("SEEDBOX_LEASE_FIXTURE_DIRECTORY", directory)
            .env("SEEDBOX_LEASE_FIXTURE_ACQUIRED", acquired)
            .env(
                "SEEDBOX_LEASE_FIXTURE_WAITING",
                acquired.with_extension("waiting"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    )
}
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "lease child did not acquire {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn independent_binaries_exclude_each_other_and_release_after_exit_or_unwind() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let first = child(root, &root.join("first"));
    wait_for(&root.join("first"));
    let second = child(root, &root.join("second"));
    wait_for(&root.join("second.waiting"));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!root.join("second").exists());
    first.release();
    wait_for(&root.join("second"));
    second.release();
    let panic = std::panic::catch_unwind(|| {
        let _lease = KernelLease::acquire(root).unwrap();
        panic!("release on unwind");
    });
    assert!(panic.is_err());
    let third = child(root, &root.join("third"));
    wait_for(&root.join("third"));
    drop(third);
    let fourth = child(root, &root.join("fourth"));
    wait_for(&root.join("fourth"));
    fourth.release();
}
