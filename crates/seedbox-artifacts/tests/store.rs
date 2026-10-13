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
    let original_modified = fs::metadata(&object).unwrap().modified().unwrap();
    store.clone().verify(&digest).unwrap();

    // Restoring mtime and keeping the length cannot conceal a write: ctime
    // changes too. This exercises the shared cache populated by put().
    fs::write(&object, b"modified").unwrap();
    fs::File::options()
        .write(true)
        .open(&object)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(original_modified))
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
fn owned_objects_are_reclaimable_until_reused_as_cache_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");
    let ledger = directory.path().join("ledger");
    let store = ContentStore::new(&cache).with_owned_ledger(&ledger);
    let input = store.put(b"cache input").unwrap();
    // Owned puts of bytes the cache already holds leave them cache inputs.
    assert_eq!(store.put_reader_owned(&b"cache input"[..]).unwrap(), input);
    let file = directory.path().join("state");
    fs::write(&file, b"captured state").unwrap();
    let state = store.put_file_owned(&file).unwrap();
    let edited = store.put_reader_owned(&b"edited archive"[..]).unwrap();
    let adopted = store.put_reader_owned(&b"later an input"[..]).unwrap();
    assert_eq!(store.put(b"later an input").unwrap(), adopted);
    assert_eq!(
        store.owned_objects(),
        [state.clone(), edited.clone()].into()
    );
    let markers: std::collections::BTreeSet<_> = fs::read_dir(&ledger)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        markers,
        [state.hex().to_owned(), edited.hex().to_owned()].into()
    );
    assert_eq!(
        store.reclaim_owned(&[edited.clone()].into()).unwrap(),
        vec![state.clone()]
    );
    assert!(store.verify(&state).is_err());
    assert!(!ledger.join(state.hex()).exists());
    store.verify(&edited).unwrap();
    store.verify(&input).unwrap();
    store.verify(&adopted).unwrap();
    // Putting reclaimed bytes again recreates an owned object.
    assert_eq!(store.put_file_owned(&file).unwrap(), state);
    store.verify(&state).unwrap();
    assert!(store.owned_objects().contains(&state));
}

#[test]
fn a_crashed_owners_ledger_reclaims_exactly_its_objects() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");
    let ledger = directory.path().join("ledger");
    let store = ContentStore::new(&cache).with_owned_ledger(&ledger);
    let input = store.put(b"cache input").unwrap();
    let owned = store.put_reader_owned(&b"owned state"[..]).unwrap();
    // A marker whose object was never named, and a stray temporary file.
    fs::write(ledger.join(ContentDigest::of(b"unnamed").hex()), b"").unwrap();
    fs::write(ledger.join(".tmp-marker"), b"").unwrap();
    drop(store);
    assert_eq!(ContentStore::reclaim_ledger(&cache, &ledger).unwrap(), 1);
    assert!(!ledger.exists());
    let restarted = ContentStore::new(&cache);
    restarted.verify(&input).unwrap();
    assert!(restarted.verify(&owned).is_err());
    assert_eq!(ContentStore::reclaim_ledger(&cache, &ledger).unwrap(), 0);
}

#[test]
fn stores_without_a_ledger_keep_owned_puts_as_cache_inputs() {
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
    let store = ContentStore::new(directory.path().join("cache"))
        .with_owned_ledger(directory.path().join("ledger"));
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
