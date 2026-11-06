use super::*;

// Independent chunk-by-chunk observation of the digest contract: read each
// chunk's stable bytes and hash them. Keep this deliberately simple so shared
// and cached chunk digests must agree with all actual bytes.
fn reference_digest(store: &Store, torn: bool) -> String {
    let mut volatile = Sha256::new();
    let mut count = 0u64;
    let (stable, _) = store.crashed_keeping(|sector, bytes| {
        volatile.update(sector.to_le_bytes());
        volatile.update(bytes);
        count += 1;
        false
    });
    let mut digest = Sha256::new();
    digest.update(b"seedbox-disk-state-v1");
    digest.update(store.capacity().to_le_bytes());
    digest.update([u8::from(torn)]);
    for start in (0..stable.capacity()).step_by(CHUNK as usize) {
        let mut chunk = vec![0; CHUNK.min(stable.capacity() - start) as usize * SECTOR];
        stable.read(start, &mut chunk).unwrap();
        digest.update(Sha256::digest(&chunk));
    }
    digest.update(count.to_le_bytes());
    digest.update(volatile.finalize());
    crate::machine::hex(&digest.finalize())
}

fn check(store: &Store) {
    for torn in [false, true] {
        assert_eq!(
            store.state_digest(torn).unwrap(),
            reference_digest(store, torn)
        );
    }
}

fn digest(store: &Store) -> String {
    store.state_digest(false).unwrap()
}

/// Three chunks, the last one partial.
const SECTORS: u64 = 2 * CHUNK + 259;

/// Distinct bytes within each sector, so skipped or shifted ranges cannot
/// pass by accident.
fn image() -> Vec<u8> {
    (0..SECTORS as usize * SECTOR)
        .map(|i| (i.wrapping_mul(17) ^ (i / SECTOR)) as u8)
        .collect()
}

#[test]
fn digest_matches_chunk_reads_across_base_overlay_and_chunk_boundaries() {
    for mut store in [
        Store::zeroed(SECTORS).unwrap(),
        Store::from_image(image()).unwrap(),
    ] {
        check(&store);
        // Both edges of each chunk, the last sector and one inside a chunk.
        for index in [
            SECTORS - 1,
            CHUNK,
            0,
            2 * CHUNK - 1,
            128,
            2 * CHUNK,
            CHUNK - 1,
        ] {
            store.write(index, &[index as u8; SECTOR]).unwrap();
        }
        store.flush();
        check(&store);
        // Include a zero overwrite, an equal overwrite and a later replacement
        // at an existing durable sector. Only the volatile stream sees them.
        store.write(CHUNK, &[0; SECTOR]).unwrap();
        store.write(128, &[128; SECTOR]).unwrap();
        store.write(CHUNK, &[3; SECTOR]).unwrap();
        // One write across a chunk boundary.
        store.write(2 * CHUNK - 1, &[9; 2 * SECTOR]).unwrap();
        let captured = store.clone();
        check(&store);
        store.flush();
        check(&store);
        check(&captured);
        let (crashed, _) = captured.crash(Some((42, "disk")), 7).unwrap();
        check(&crashed);
        assert_ne!(digest(&captured), digest(&store));
    }
}

#[test]
fn equal_contents_give_equal_digests_however_they_were_written() {
    let image = image();
    let zero = Store::zeroed(SECTORS).unwrap();
    let based = Store::from_image(image.clone()).unwrap();
    // Durable zero overwrites and an all-zero image equal an untouched zero
    // base, so a written zero chunk takes the same digest as an unwritten one.
    let mut zeroed = zero.clone();
    for index in [0, CHUNK - 1, CHUNK, SECTORS - 1] {
        zeroed.write(index, &[0; SECTOR]).unwrap();
    }
    zeroed.flush();
    let blank = Store::from_image(vec![0; image.len()]).unwrap();
    assert_eq!(digest(&zeroed), digest(&zero));
    assert_eq!(digest(&blank), digest(&zero));
    // The image's bytes written over a zero base equal the image, whether in
    // one write or sector by sector backwards across several flushes.
    let mut whole = zero.clone();
    whole.write(0, &image).unwrap();
    whole.flush();
    let mut backwards = zero.clone();
    for (index, sector) in image.chunks(SECTOR).enumerate().rev() {
        backwards.write(index as u64, sector).unwrap();
        if index % 1000 == 0 {
            backwards.flush();
        }
    }
    assert_eq!(digest(&whole), digest(&based));
    assert_eq!(digest(&backwards), digest(&based));
    // Rewriting an image's own bytes, here across a chunk boundary, and
    // replacing an earlier durable write change nothing either.
    let mut rewritten = based.clone();
    let at = (CHUNK - 1) as usize * SECTOR;
    rewritten
        .write(CHUNK - 1, &image[at..at + 2 * SECTOR])
        .unwrap();
    rewritten.write(5, &[1; SECTOR]).unwrap();
    rewritten.flush();
    rewritten.write(5, &image[5 * SECTOR..6 * SECTOR]).unwrap();
    rewritten.flush();
    assert_eq!(digest(&rewritten), digest(&based));
    // One changed byte in any chunk changes the digest.
    for index in [0, CHUNK + 7, SECTORS - 1] {
        let mut changed = based.clone();
        let at = index as usize * SECTOR;
        let mut sector = image[at..at + SECTOR].to_vec();
        sector[SECTOR - 1] ^= 1;
        changed.write(index, &sector).unwrap();
        changed.flush();
        assert_ne!(digest(&changed), digest(&based));
    }
}

#[test]
fn image_chunk_digests_are_computed_once_and_shared_by_captures() {
    let cached = |store: &Store| match &store.base {
        Base::Image(_, chunks) => chunks.get().is_some(),
        Base::Zero => false,
    };
    let mut store = Store::from_image(image()).unwrap();
    let capture = store.clone();
    assert!(!cached(&capture));
    let before = digest(&store);
    assert!(cached(&capture), "an earlier capture shares the digests");
    // Chunks with later durable writes are hashed again, never cached.
    store.write(CHUNK + 1, &[7; SECTOR]).unwrap();
    store.flush();
    check(&store);
    assert_ne!(digest(&store), before);
    assert_eq!(digest(&capture), before);
    // Captures racing to compute the digests all agree.
    let racing = Store::from_image(image()).unwrap();
    let digests: Vec<_> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let capture = racing.clone();
                scope.spawn(move || digest(&capture))
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    assert!(digests.iter().all(|d| *d == before), "{digests:?}");
}

#[test]
fn large_sparse_disks_digest_written_chunks_with_constant_zero_chunks() {
    // The largest machine disk (64 GiB) with one durable sector at its end.
    const CAPACITY: u64 = 64 << 21;
    let mut store = Store::zeroed(CAPACITY).unwrap();
    store.write(CAPACITY - 1, &[1; SECTOR]).unwrap();
    store.flush();
    let zero = Sha256::digest(vec![0; CHUNK as usize * SECTOR]);
    let mut last = Sha256::new();
    last.update(vec![0; (CHUNK - 1) as usize * SECTOR]);
    last.update([1; SECTOR]);
    let mut expected = Sha256::new();
    expected.update(b"seedbox-disk-state-v1");
    expected.update(CAPACITY.to_le_bytes());
    expected.update([0]);
    for _ in 1..CAPACITY / CHUNK {
        expected.update(zero);
    }
    expected.update(last.finalize());
    expected.update(0u64.to_le_bytes());
    expected.update(Sha256::new().finalize());
    assert_eq!(digest(&store), crate::machine::hex(&expected.finalize()));
}
