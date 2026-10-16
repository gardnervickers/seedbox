use super::*;

fn fill(value: u8) -> Vec<u8> {
    vec![value; SECTOR]
}

/// Every sector's first byte, now and after a power loss.
fn view(store: &Store) -> (Vec<u8>, Vec<u8>) {
    let first = |store: &Store| {
        (0..store.capacity())
            .map(|s| store.sector(s)[0])
            .collect::<Vec<_>>()
    };
    let mut lost = store.clone();
    lost.discard_unflushed();
    (first(store), first(&lost))
}

/// A base with nonzero data, durable and unflushed writes (some overlapping),
/// and zero writes over nonzero data.
fn sample() -> Store {
    let mut store = Store::from_image([fill(9), fill(9), fill(9), fill(9)].concat()).unwrap();
    store.write(0, &fill(1)).unwrap();
    store.write(2, &fill(0)).unwrap();
    store.flush();
    store.write(0, &fill(3)).unwrap();
    store.write(3, &fill(0)).unwrap();
    store
}

fn base() -> Store {
    Store::from_image([fill(9), fill(9), fill(9), fill(9)].concat()).unwrap()
}

#[test]
fn overlays_round_trip_without_promoting() {
    let source = sample();
    let before = view(&source);
    let bytes = source.encode_overlays();
    assert_eq!(view(&source), before, "encoding changes nothing");
    assert_eq!(bytes.len(), 8 + 8 + 2 * 8 + 4 * ENTRY);
    let mut restored = base();
    restored.replace_overlays(&bytes).unwrap();
    assert_eq!(view(&restored), before);
    assert_eq!(before, (vec![3, 9, 0, 0], vec![1, 9, 0, 9]));
    assert_eq!(restored.encode_overlays(), bytes);
    // Replacement discards whatever overlays the destination already had.
    let mut other = sample();
    other.write(1, &fill(7)).unwrap();
    other.flush();
    other.replace_overlays(&base().encode_overlays()).unwrap();
    assert_eq!(view(&other), (vec![9; 4], vec![9; 4]));
}

#[test]
fn every_malformed_encoding_is_rejected_without_change() {
    let valid = sample().encode_overlays();
    let entry = |sector: u64| [sector.to_le_bytes().to_vec(), fill(5)].concat();
    let layer = |sectors: &[u64]| {
        let mut bytes = (sectors.len() as u64).to_le_bytes().to_vec();
        for &sector in sectors {
            bytes.extend(entry(sector));
        }
        bytes
    };
    let encode = |capacity: u64, durable: Vec<u8>, unflushed: Vec<u8>| {
        [
            MAGIC.to_vec(),
            capacity.to_le_bytes().to_vec(),
            durable,
            unflushed,
        ]
        .concat()
    };
    let mut cases = vec![
        (
            b"SBXSTOR2"
                .iter()
                .copied()
                .chain(valid[8..].iter().copied())
                .collect(),
            CodecError::Magic,
        ),
        (encode(5, layer(&[]), layer(&[])), CodecError::Capacity),
        ([valid.clone(), vec![0]].concat(), CodecError::Trailing),
        (encode(4, layer(&[1, 1]), layer(&[])), CodecError::Order),
        (encode(4, layer(&[]), layer(&[2, 1])), CodecError::Order),
        (encode(4, layer(&[4]), layer(&[])), CodecError::Range),
        (encode(4, layer(&[u64::MAX]), layer(&[])), CodecError::Range),
        (
            encode(4, layer(&[0, 1, 2, 3, 3]), layer(&[])),
            CodecError::Count,
        ),
    ];
    // Counts larger than the remaining input fail before allocation.
    for count in [2, u64::MAX] {
        let mut bytes = encode(4, layer(&[0]), layer(&[]));
        bytes[16..24].copy_from_slice(&count.to_le_bytes());
        cases.push((bytes, CodecError::Count));
    }
    // Every strict prefix of a valid encoding is incomplete.
    for length in 0..valid.len() {
        cases.push((valid[..length].to_vec(), CodecError::Truncated));
    }
    for (bytes, expected) in cases {
        let mut store = sample();
        let before = (view(&store), store.encode_overlays());
        let error = store.replace_overlays(&bytes).unwrap_err();
        // A truncated layer may surface first as a count exceeding the input.
        if expected == CodecError::Truncated {
            assert!(
                matches!(error, CodecError::Truncated | CodecError::Count),
                "{error:?}"
            );
        } else {
            assert_eq!(error, expected);
        }
        assert_eq!((view(&store), store.encode_overlays()), before);
    }
}

#[test]
fn layers_may_share_sectors_and_empty_overlays_are_minimal() {
    let mut store = Store::zeroed(2).unwrap();
    assert_eq!(store.encode_overlays().len(), 32);
    store.write(1, &fill(4)).unwrap();
    store.flush();
    store.write(1, &fill(6)).unwrap();
    let mut restored = Store::zeroed(2).unwrap();
    restored.replace_overlays(&store.encode_overlays()).unwrap();
    assert_eq!(view(&restored), (vec![0, 6], vec![0, 4]));
}
