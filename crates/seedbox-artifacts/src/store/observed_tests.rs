use super::*;

#[test]
fn cancellation_covers_hash_copy_and_verify_and_cleans_unpublished_files() {
    const SIZE: u64 = 256 * 1024;
    for (stop, published) in [
        (64 * 1024, false),
        (SIZE + 64 * 1024, false),
        (2 * SIZE + 64 * 1024, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, vec![31; SIZE as usize]).unwrap();
        let store = ContentStore::new(root.path().join("store"));
        let mut bytes = 0;
        let result = store.put_file_observed(&source, &mut |count| {
            assert!(count <= 64 * 1024);
            bytes += count;
            if bytes >= stop {
                Err(io::Error::other("cancelled by observer"))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(Error::Io(e)) if e.to_string() == "cancelled by observer"));
        assert_eq!(bytes, stop);
        let directory = store.root.join("sha256");
        let entries: Vec<_> = if directory.exists() {
            fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect()
        } else {
            vec![]
        };
        assert_eq!(entries.len(), usize::from(published));
        for entry in entries {
            let digest = ContentDigest::try_from(entry.file_name().into_string().unwrap()).unwrap();
            store.verify(&digest).unwrap();
        }
    }
}

#[test]
fn cache_hits_still_observe_cancellation_and_existing_corruption_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::write(&source, b"original").unwrap();
    let store = ContentStore::new(root.path().join("store"));
    let digest = store.put_file(&source).unwrap();
    assert!(store
        .put_file_observed(&source, &mut |_| Err(io::Error::other("cancelled")))
        .is_err());
    fs::write(store.path(&digest), b"corrupt!").unwrap();
    let mut processed = 0;
    assert!(matches!(
        store.put_file_observed(&source, &mut |n| {
            processed += n;
            Ok(())
        }),
        Err(Error::Integrity(_))
    ));
    assert!(processed > 0, "existing object must be reverified");
}
