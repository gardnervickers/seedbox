use super::*;
use crate::store::ContentDigest;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Condvar, Mutex,
};
use std::time::Duration;

fn artifact(root: &std::path::Path, name: &str, bytes: &[u8]) -> wire::Artifact {
    let path = root.join(name);
    std::fs::write(&path, bytes).unwrap();
    wire::Artifact {
        sha256: ContentDigest::of(bytes).hex().into(),
        length: bytes.len() as u64,
        artifact: path.to_str().unwrap().into(),
    }
}
fn prepared(root: &std::path::Path) -> wire::Prepared {
    let archive = tar::Builder::new(Vec::new()).into_inner().unwrap();
    let filesystem = artifact(root, "filesystem", &archive);
    let machine = |name: &str| wire::Machine {
        name: name.into(),
        memory_bytes: 128 << 20,
        filesystem: filesystem.clone(),
        process: wire::Process {
            argv: vec![format!("/{name}")],
            environment: Default::default(),
            working_directory: "/".into(),
            user: "0:0".into(),
        },
        disk: None,
        interface: None,
    };
    wire::Prepared {
        inventory: wire::Inventory::Tcg {
            policy: None,
            executable: filesystem.clone(),
            kernel: filesystem.clone(),
            init: filesystem.clone(),
            command_line: filesystem.clone(),
            firmware: filesystem.clone(),
            runtime: filesystem.clone(),
            formatter: None,
            environments: Default::default(),
            boot_archives: Default::default(),
            epoch_seconds: 86400,
        },
        oracle: machine("z"),
        machines: vec![machine("a"), machine("b")],
        spawnable: vec![machine("later-z"), machine("later-a")],
        quantum_ns: 1_000_000,
        network: Default::default(),
    }
}

#[test]
fn exact_definition_contents_and_roles_keep_original_order() {
    let root = tempfile::tempdir().unwrap();
    let mut input = prepared(root.path());
    input.machines[0].disk = Some(wire::Disk::Image {
        contents: artifact(root.path(), "disk", &[7; 1024]),
    });
    input.spawnable[0].disk = Some(wire::Disk::Zero { bytes: 512 });
    let expected = (
        super::super::definition(&input.oracle).unwrap(),
        input
            .machines
            .iter()
            .map(super::super::definition)
            .collect::<Result<Vec<_>>>()
            .unwrap(),
        input
            .spawnable
            .iter()
            .map(super::super::definition)
            .collect::<Result<Vec<_>>>()
            .unwrap(),
    );
    for workers in [1, 2, 5, 99] {
        assert_eq!(definitions(&input, workers).unwrap(), expected);
    }
}

struct Completed<'a>(&'a AtomicUsize, &'a AtomicUsize);
impl Drop for Completed<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn bounded_overlap_joins_every_occurrence_and_keeps_first_error_on_panic() {
    let root = tempfile::tempdir().unwrap();
    let input = prepared(root.path());
    let inputs: Vec<_> = std::iter::once(&input.oracle)
        .chain(&input.machines)
        .chain(&input.spawnable)
        .collect();
    for failure in [0, 1, 2] {
        let entered = (Mutex::new(0), Condvar::new());
        let active = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let result = convert(&inputs, 3, &|machine| {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(count, Ordering::SeqCst);
            let _completed = Completed(&active, &completed);
            // Only the first input assigned to each worker participates. A
            // positive entry signal, never timed absence, proves concurrency.
            if ["z", "a", "b"].contains(&machine.name.as_str()) {
                let (lock, signal) = &entered;
                let mut count = lock.lock().unwrap();
                *count += 1;
                signal.notify_all();
                let (count, _) = signal
                    .wait_timeout_while(count, Duration::from_secs(30), |n| *n < 3)
                    .unwrap();
                assert_eq!(*count, 3, "all three converters must enter");
            }
            match (failure, machine.name.as_str()) {
                (1, "z") | (2, "a") => return Err(Error::Integrity("first ordinary error".into())),
                (1, "a") | (2, "z") => panic!("conversion fixture panic"),
                _ => {}
            }
            Ok(machine.name.clone())
        });
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(completed.load(Ordering::SeqCst), 5);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        match failure {
            0 => assert_eq!(result.unwrap(), ["z", "a", "b", "later-z", "later-a"]),
            1 => assert_eq!(
                result.unwrap_err().to_string(),
                "input integrity: first ordinary error"
            ),
            2 => assert!(result.unwrap_err().to_string().contains("for z panicked")),
            _ => unreachable!(),
        }
    }
}

#[test]
fn unchanged_sha_archive_and_disk_validation_rejects_original_first_bad_input() {
    let root = tempfile::tempdir().unwrap();
    let original = prepared(root.path());
    let mut invalid = original.clone();
    invalid.machines[0].filesystem = artifact(root.path(), "bad-tar", b"not a tar archive");
    invalid.machines[1].disk = Some(wire::Disk::Image {
        contents: artifact(root.path(), "bad-disk", &[0; 513]),
    });
    let mut cases = vec![invalid.clone()];
    invalid.machines[0] = original.machines[0].clone();
    cases.push(invalid.clone());
    invalid.machines[1].disk = Some(wire::Disk::Zero { bytes: 0 });
    cases.push(invalid);
    let mut bad_sha = original;
    bad_sha.oracle.filesystem.sha256 = "00".repeat(32);
    cases.push(bad_sha);
    for input in cases {
        let expected = std::iter::once(&input.oracle)
            .chain(&input.machines)
            .chain(&input.spawnable)
            .map(super::super::definition)
            .collect::<Result<Vec<_>>>()
            .unwrap_err()
            .to_string();
        for workers in [1, 3, 5] {
            assert_eq!(
                definitions(&input, workers).unwrap_err().to_string(),
                expected
            );
        }
    }
}
