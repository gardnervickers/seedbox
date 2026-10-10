//! Content-store recovery, integrity and bounded streaming.
use seedbox_artifacts::store::{ContentDigest, ContentStore};
use std::fs;

#[test]
fn unsynced_put_replaces_an_object_a_host_crash_truncated() {
    // Unsynced objects can survive a host crash renamed into place but empty
    // or short. The same bytes put again must not fail on that object forever.
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = b"{\"identity\":1}";
    let digest = store.put_unsynced(bytes).unwrap();
    let object = directory.path().join("sha256").join(digest.hex());
    std::fs::write(&object, b"").unwrap();
    assert!(store.verify(&digest).is_err());
    assert_eq!(store.put_unsynced(bytes).unwrap(), digest);
    store.verify(&digest).unwrap();
    std::fs::write(&object, b"").unwrap();
    let file = directory.path().join("log");
    std::fs::write(&file, bytes).unwrap();
    assert_eq!(store.put_file_unsynced(&file).unwrap(), digest);
    store.verify(&digest).unwrap();
}

#[test]
fn content_store_never_silently_repairs_corruption_or_accepts_path_digests() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let hash = store.put(b"input").unwrap();
    let copied = tempfile::tempdir().unwrap();
    let private = store.materialize(&hash, copied.path()).unwrap();
    fs::write(&private, b"backend changed its private input").unwrap();
    assert_eq!(store.read(&hash).unwrap(), b"input");
    fs::write(directory.path().join("sha256").join(hash.hex()), b"corrupt").unwrap();
    assert!(store.put(b"input").is_err());
    assert!(serde_json::from_str::<ContentDigest>("\"../../outside\"").is_err());
}

#[test]
fn large_artifacts_stream_with_bounded_reads_and_corrupt_copies_are_not_published() {
    use std::io::{self, Read};
    struct BoundedSource(usize);
    impl Read for BoundedSource {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(buffer.len() <= 64 * 1024);
            let count = buffer.len().min(self.0);
            buffer[..count].fill(7);
            self.0 -= count;
            Ok(count)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let size = 12 * 1024 * 1024;
    let digest = store.put_reader(BoundedSource(size)).unwrap();
    assert!(store
        .read(&digest)
        .unwrap_err()
        .to_string()
        .contains("read limit"));
    let output = directory.path().join("inputs");
    let path = store.materialize(&digest, &output).unwrap();
    assert_eq!(fs::metadata(path).unwrap().len(), size as u64);
    fs::write(
        directory.path().join("store/sha256").join(digest.hex()),
        b"corrupt",
    )
    .unwrap();
    let bad_output = directory.path().join("bad-inputs");
    assert!(store.materialize(&digest, &bad_output).is_err());
    assert_eq!(fs::read_dir(bad_output).unwrap().count(), 0);
}
