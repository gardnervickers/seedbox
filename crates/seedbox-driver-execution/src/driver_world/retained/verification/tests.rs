use super::*;
use crate::store::ContentDigest;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Condvar, Mutex,
};
use std::time::Duration;

fn artifact(path: &std::path::Path, bytes: &[u8]) -> Artifact {
    Artifact {
        sha256: ContentDigest::of(bytes).hex().into(),
        length: bytes.len() as u64,
        artifact: path.to_str().unwrap().into(),
    }
}

#[test]
fn all_checks_overlap_and_finish_before_the_first_ordered_error_returns() {
    let directory = tempfile::tempdir().unwrap();
    let inputs: Vec<_> = (0..6)
        .map(|index| artifact(&directory.path().join(index.to_string()), b"fixture"))
        .collect();
    let gate = (Mutex::new(0), Condvar::new());
    let checks = AtomicUsize::new(0);
    let error = verify(&inputs, 3, &|input| {
        let index: usize = std::path::Path::new(&input.artifact)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        checks.fetch_add(1, Ordering::SeqCst);
        if index < 3 {
            let (lock, signal) = &gate;
            let mut entered = lock.lock().unwrap();
            *entered += 1;
            signal.notify_all();
            let (entered, _) = signal
                .wait_timeout_while(entered, Duration::from_secs(30), |entered| *entered < 3)
                .unwrap();
            assert_eq!(*entered, 3, "all three verifiers must enter");
        }
        match index {
            0 => Err(Error::Integrity("first artifact".into())),
            1 => panic!("later verifier panic"),
            _ => Ok(()),
        }
    })
    .unwrap_err();
    assert!(error.to_string().contains("first artifact"));
    assert_eq!(checks.load(Ordering::SeqCst), inputs.len());
}

#[test]
fn actual_bytes_lengths_and_replaced_or_mutated_files_are_verified() {
    let directory = tempfile::tempdir().unwrap();
    let paths: Vec<_> = (0..4)
        .map(|index| directory.path().join(index.to_string()))
        .collect();
    let inputs: Vec<_> = paths
        .iter()
        .map(|path| {
            std::fs::write(path, b"original").unwrap();
            artifact(path, b"original")
        })
        .collect();
    for workers in [1, 4] {
        artifacts(&inputs, workers).unwrap();
        std::fs::write(&paths[2], b"changed!").unwrap();
        assert!(artifacts(&inputs, workers)
            .unwrap_err()
            .to_string()
            .contains("SHA-256"));
        std::fs::write(&paths[2], b"short").unwrap();
        assert!(artifacts(&inputs, workers)
            .unwrap_err()
            .to_string()
            .contains("encoded length"));
        std::fs::remove_file(&paths[2]).unwrap();
        std::fs::write(&paths[2], b"replaced").unwrap();
        assert!(artifacts(&inputs, workers)
            .unwrap_err()
            .to_string()
            .contains("SHA-256"));
        std::fs::write(&paths[2], b"original").unwrap();
    }
}
