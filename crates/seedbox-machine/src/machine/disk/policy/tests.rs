use super::*;

// Independent sector-by-sector observation of the digest contract. Keep this
// deliberately simple so optimized traversal must agree on all actual bytes.
fn sector_digest(store: &Store, torn: bool) -> String {
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
    let mut sector = [0; SECTOR];
    for index in 0..stable.capacity() {
        stable.read(index, &mut sector).unwrap();
        digest.update(sector);
    }
    digest.update(count.to_le_bytes());
    digest.update(volatile.finalize());
    crate::machine::hex(&digest.finalize())
}

fn check(store: &Store) {
    for torn in [false, true] {
        assert_eq!(
            store.state_digest(torn).unwrap(),
            sector_digest(store, torn)
        );
    }
}

#[test]
fn digest_stream_matches_sector_reads_across_base_and_overlay_boundaries() {
    // Cross two zero-chunk boundaries and include distinct bytes within each
    // image sector, so skipped or shifted base ranges cannot pass by accident.
    const SECTORS: usize = 259;
    let image: Vec<_> = (0..SECTORS * SECTOR)
        .map(|i| (i.wrapping_mul(17) ^ (i / SECTOR)) as u8)
        .collect();
    for mut store in [
        Store::zeroed(SECTORS as u64).unwrap(),
        Store::from_image(image).unwrap(),
    ] {
        check(&store);
        for index in [258, 128, 0, 129, 127, 257, 1] {
            store.write(index, &[index as u8; SECTOR]).unwrap();
        }
        store.flush();
        check(&store);
        // Include a zero overwrite, an equal overwrite and a later replacement
        // at an existing durable sector. Only the volatile stream sees them.
        store.write(128, &[0; SECTOR]).unwrap();
        store.write(127, &[127; SECTOR]).unwrap();
        store.write(128, &[3; SECTOR]).unwrap();
        store.write(2, &[9; 2 * SECTOR]).unwrap();
        let captured = store.clone();
        check(&store);
        store.flush();
        check(&store);
        check(&captured);
        let (crashed, _) = captured.crash(Some((42, "disk")), 7).unwrap();
        check(&crashed);
        assert_ne!(captured.state_digest(false), store.state_digest(false));
    }
}
