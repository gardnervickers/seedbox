//! Content-store recovery, integrity and bounded streaming.
use seedbox_artifacts::store::{ContentDigest, ContentStore};
use seedbox_test_time::HangGuard;
use std::{fs, os::unix::fs::MetadataExt, time::Duration};

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

#[test]
fn split_store_keeps_inputs_and_derivation_cache_but_discards_session_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");
    let session = directory.path().join("session");
    let store = ContentStore::with_ephemeral_evidence(&cache, &session);
    let binary = store.put(b"immutable binary").unwrap();
    let identity = store.put_unsynced(b"run identity").unwrap();
    let file = directory.path().join("application.log");
    fs::write(&file, b"application output").unwrap();
    let log = store.put_file_unsynced(&file).unwrap();
    let record = ContentDigest::of(b"derivation cache key");
    store.set_record(&record, &binary).unwrap();
    assert_eq!(store.read(&identity).unwrap(), b"run identity");
    assert_eq!(store.size(&log).unwrap(), 18);
    assert!(!cache.join("sha256").join(identity.hex()).exists());
    assert!(!cache.join("sha256").join(log.hex()).exists());
    // Retaining an input whose bytes appeared as evidence must still populate
    // the persistent cache, rather than reuse only the ephemeral copy.
    let promoted = store.put_unsynced(b"later becomes an input").unwrap();
    fs::write(&file, b"later becomes an input").unwrap();
    assert_eq!(store.put_file(&file).unwrap(), promoted);
    fs::remove_dir_all(&session).unwrap();
    let restarted = ContentStore::with_ephemeral_evidence(&cache, &session);
    assert_eq!(restarted.read(&binary).unwrap(), b"immutable binary");
    assert_eq!(
        restarted.read(&promoted).unwrap(),
        b"later becomes an input"
    );
    assert_eq!(restarted.record(&record), Some(binary));
    assert!(restarted.read(&identity).is_err());
    assert!(restarted.read(&log).is_err());
}

#[test]
fn session_copy_cannot_hide_a_corrupt_persistent_binary() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");
    let session = directory.path().join("session");
    let store = ContentStore::with_ephemeral_evidence(&cache, &session);
    let bytes = b"same binary in both stores";
    let digest = store.put(bytes).unwrap();
    assert_eq!(store.put_unsynced(bytes).unwrap(), digest);
    fs::write(cache.join("sha256").join(digest.hex()), b"corrupt").unwrap();
    let source = directory.path().join("source");
    fs::write(&source, bytes).unwrap();
    assert!(store.read(&digest).is_err());
    assert!(store.put_file(&source).is_err());
    assert!(store.put(bytes).is_err());
}

#[test]
fn cached_verification_rejects_same_length_edits_and_replaced_objects() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let digest = store.put(b"original").unwrap();
    let object = directory.path().join("sha256").join(digest.hex());
    let original = fs::metadata(&object).unwrap();
    let original_modified = original.modified().unwrap();
    let original_changed = (original.ctime(), original.ctime_nsec());
    store.clone().verify(&digest).unwrap();

    // Exercise ctime invalidation with mtime and length unchanged. Some
    // filesystems update ctime on coarse ticks, so observe the change before
    // checking the shared cache populated by put().
    fs::write(&object, b"modified").unwrap();
    let file = fs::File::options().write(true).open(&object).unwrap();
    HangGuard::new(
        "content-store corruption changes ctime",
        Duration::from_secs(30),
    )
    .wait_for(|| {
        file.set_times(fs::FileTimes::new().set_modified(original_modified))
            .unwrap();
        let changed = file.metadata().unwrap();
        ((changed.ctime(), changed.ctime_nsec()) != original_changed).then_some(())
    })
    .unwrap();
    assert!(store.verify(&digest).is_err());

    fs::write(&object, b"original").unwrap();
    store.verify(&digest).unwrap();
    let replacement = directory.path().join("replacement");
    fs::write(&replacement, b"replaced").unwrap();
    fs::rename(replacement, &object).unwrap();
    assert!(store.clone().verify(&digest).is_err());
    fs::remove_file(object).unwrap();
    assert!(store.verify(&digest).is_err());
}

#[test]
fn put_file_hashes_same_length_source_rewrites() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let store = ContentStore::new(directory.path().join("store"));
    fs::write(&source, b"original").unwrap();
    let first = store.put_file(&source).unwrap();

    fs::write(&source, b"modified").unwrap();
    let second = store.put_file(&source).unwrap();
    assert_eq!(second, ContentDigest::of(b"modified"));
    assert_ne!(first, second);
    assert_eq!(store.read(&first).unwrap(), b"original");
    assert_eq!(store.read(&second).unwrap(), b"modified");
}

fn put_file_owned(store: &ContentStore, path: &std::path::Path) -> ContentDigest {
    store
        .put_file_owned_observed(path, &mut |_| Ok(()))
        .unwrap()
}

#[test]
fn owned_objects_are_reclaimable_until_reused_as_cache_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");
    let store = ContentStore::new(&cache).with_owned_objects();
    let input = store.put(b"cache input").unwrap();
    // Owned puts of bytes the cache already holds leave them cache inputs.
    assert_eq!(store.put_reader_owned(&b"cache input"[..]).unwrap(), input);
    let file = directory.path().join("state");
    fs::write(&file, b"captured state").unwrap();
    let state = put_file_owned(&store, &file);
    let imported = store.put_reader_owned(&b"imported binary"[..]).unwrap();
    let adopted = store.put_reader_owned(&b"later an input"[..]).unwrap();
    assert_eq!(store.put(b"later an input").unwrap(), adopted);
    let reused = store.put_reader_owned(&b"later a file input"[..]).unwrap();
    let source = directory.path().join("input");
    fs::write(&source, b"later a file input").unwrap();
    assert_eq!(store.put_file(&source).unwrap(), reused);
    assert_eq!(
        store.owned_objects(),
        [state.clone(), imported.clone()].into()
    );
    assert_eq!(
        store.reclaim_owned(&[imported.clone()].into()).unwrap(),
        vec![state.clone()]
    );
    assert!(store.verify(&state).is_err());
    for kept in [&imported, &input, &adopted, &reused] {
        store.verify(kept).unwrap();
    }
    // Putting reclaimed bytes again recreates an owned object.
    assert_eq!(put_file_owned(&store, &file), state);
    store.verify(&state).unwrap();
    assert!(store.owned_objects().contains(&state));
}

/// Ownership lives only in the process that created the objects: another
/// store on the same cache, such as the next server after a crash, sees
/// ordinary cache content and never reclaims it.
#[test]
fn owned_objects_outlive_their_owner_as_cache_content() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");
    let store = ContentStore::new(&cache).with_owned_objects();
    let owned = store.put_reader_owned(&b"owned state"[..]).unwrap();
    drop(store);
    let restarted = ContentStore::new(&cache).with_owned_objects();
    assert!(restarted.owned_objects().is_empty());
    assert!(restarted
        .reclaim_owned(&Default::default())
        .unwrap()
        .is_empty());
    restarted.verify(&owned).unwrap();
    // Putting it again does not make it owned: it already exists.
    assert_eq!(
        restarted.put_reader_owned(&b"owned state"[..]).unwrap(),
        owned
    );
    assert!(restarted.owned_objects().is_empty());
}

#[test]
fn stores_without_ownership_keep_owned_puts_as_cache_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let digest = store.put_reader_owned(&b"state"[..]).unwrap();
    assert!(store.owned_objects().is_empty());
    assert!(store.reclaim_owned(&Default::default()).unwrap().is_empty());
    store.verify(&digest).unwrap();
}

#[test]
fn reclamation_never_removes_an_object_an_ordinary_put_returned() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("cache")).with_owned_objects();
    for round in 0..200u32 {
        let bytes = format!("racing object {round}").into_bytes();
        let digest = std::thread::scope(|scope| {
            let input = scope.spawn(|| store.put(&bytes).unwrap());
            // Without the caller's exclusion an owned put can lose its object.
            scope.spawn(|| store.put_reader_owned(bytes.as_slice()).ok());
            scope.spawn(|| store.reclaim_owned(&Default::default()).unwrap());
            input.join().unwrap()
        });
        store.reclaim_owned(&Default::default()).unwrap();
        store.verify(&digest).unwrap();
    }
    assert!(store.owned_objects().is_empty());
}
