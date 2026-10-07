use seedbox::{execution::CancellationToken, store::ContentStore, Error};
use std::{
    fs,
    io::{self, Write},
};

#[test]
fn checked_copy_stops_after_cancellation_without_changing_retained_bytes() {
    struct CancelWrite {
        bytes: usize,
        token: CancellationToken,
    }
    impl Write for CancelWrite {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes += bytes.len();
            self.token.cancel();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let store = ContentStore::new(root.path());
    let data = vec![42; 1024 * 1024];
    let digest = store.put(&data).unwrap();
    let token = CancellationToken::default();
    let mut output = CancelWrite {
        bytes: 0,
        token: token.clone(),
    };
    let result = store.copy_to_checked(&digest, &mut output, &|| {
        if token.is_cancelled() {
            Err(Error::PreparationCancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::PreparationCancelled)));
    assert!(output.bytes > 0 && output.bytes < data.len());
    assert_eq!(store.read(&digest).unwrap(), data);
}

#[test]
fn cancelled_materialization_does_not_publish_or_leave_partial_files() {
    let root = tempfile::tempdir().unwrap();
    let store = ContentStore::new(root.path().join("store"));
    let data = vec![17; 1024 * 1024];
    let digest = store.put(&data).unwrap();
    let output = root.path().join("output");
    let result = store.materialize_checked(&digest, &output, &|| {
        // Observe an in-progress copy, as a cancellation watcher would; no
        // dependency on the number or ordering of internal callback calls.
        let copying = output.exists()
            && fs::read_dir(&output)
                .unwrap()
                .any(|entry| entry.unwrap().metadata().unwrap().len() > 0);
        if copying {
            Err(Error::PreparationCancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::PreparationCancelled)));
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
    assert_eq!(store.read(&digest).unwrap(), data);
}
