use super::*;

mod reference;

fn fill(value: u8, sectors: usize) -> Vec<u8> {
    vec![value; sectors * SECTOR]
}

/// The uniform value of one sector; every test sector is written uniformly.
fn at(store: &Store, sector: u64) -> u8 {
    let mut buffer = [0xa5; SECTOR];
    store.read(sector, &mut buffer).unwrap();
    assert!(
        buffer.iter().all(|&b| b == buffer[0]),
        "sector {sector} torn"
    );
    buffer[0]
}

fn all(store: &Store) -> Vec<u8> {
    (0..store.capacity()).map(|s| at(store, s)).collect()
}

/// What a power loss would leave, observed on an owned capture.
fn after_loss(store: &Store) -> Vec<u8> {
    let mut capture = store.clone();
    capture.discard_unflushed();
    all(&capture)
}

#[test]
fn constructors_reject_empty_or_partial_bases() {
    assert_eq!(Store::zeroed(0).err(), Some(Error::Empty));
    assert_eq!(Store::from_image(vec![]).err(), Some(Error::Empty));
    assert_eq!(
        Store::from_image(vec![1; 100]).err(),
        Some(Error::Alignment)
    );
    assert_eq!(
        Store::from_image(vec![1; SECTOR + 1]).err(),
        Some(Error::Alignment)
    );
    let image = Store::from_image([fill(3, 1), fill(4, 2)].concat()).unwrap();
    assert_eq!((image.capacity(), all(&image)), (3, vec![3, 4, 4]));
    // The last addressable sector is usable without index overflow.
    let mut zero = Store::zeroed(u64::MAX).unwrap();
    assert_eq!(at(&zero, u64::MAX - 1), 0);
    zero.write(u64::MAX - 1, &fill(3, 1)).unwrap();
    assert_eq!(at(&zero, u64::MAX - 1), 3);
    assert_eq!(zero.write(u64::MAX - 1, &fill(3, 2)), Err(Error::Range));
}

#[test]
fn shared_images_are_not_copied_or_changed() {
    let image: Arc<[u8]> = fill(4, 2).into();
    let mut store = Store::from_image(image.clone()).unwrap();
    store.write(0, &fill(5, 1)).unwrap();
    store.flush();
    assert_eq!((all(&store), &image[..]), (vec![5, 4], &fill(4, 2)[..]));
    // The store and its capture hold the caller's allocation, not a copy.
    let capture = store.clone();
    assert_eq!(Arc::strong_count(&image), 3);
    drop((store, capture));
    assert_eq!(Arc::strong_count(&image), 1);
}

#[test]
fn completed_writes_are_visible_at_once_across_all_layers() {
    let mut store = Store::from_image(fill(1, 6)).unwrap();
    store.write(1, &fill(2, 2)).unwrap();
    store.flush();
    store.write(2, &fill(3, 2)).unwrap();
    // One read spans base, durable, unflushed-over-durable and unflushed.
    let mut buffer = vec![0; 5 * SECTOR];
    store.read(0, &mut buffer).unwrap();
    let values: Vec<u8> = buffer.chunks(SECTOR).map(|s| s[0]).collect();
    assert_eq!(values, [1, 2, 3, 3, 1]);
    // A later write in the same sector replaces the earlier unflushed one.
    store.write(3, &fill(4, 1)).unwrap();
    assert_eq!(all(&store), [1, 2, 3, 4, 1, 1]);
    assert_eq!(after_loss(&store), [1, 2, 2, 1, 1, 1]);
}

#[test]
fn flush_covers_every_earlier_write_and_none_after() {
    let mut store = Store::zeroed(8).unwrap();
    store.write(1, &fill(1, 2)).unwrap();
    store.write(5, &fill(2, 1)).unwrap();
    store.write(1, &fill(3, 1)).unwrap();
    store.flush();
    store.write(2, &fill(4, 1)).unwrap();
    store.write(6, &fill(5, 1)).unwrap();
    assert_eq!(after_loss(&store), [0, 3, 1, 0, 0, 2, 0, 0]);
    store.discard_unflushed();
    assert_eq!(all(&store), [0, 3, 1, 0, 0, 2, 0, 0]);
    // Discarding twice, or flushing nothing, changes nothing.
    store.discard_unflushed();
    store.flush();
    assert_eq!(after_loss(&store), all(&store));
}

#[test]
fn zero_writes_overwrite_nonzero_base_and_durable_sectors() {
    let mut store = Store::from_image(fill(9, 3)).unwrap();
    store.write(0, &fill(0, 2)).unwrap();
    assert_eq!(all(&store), [0, 0, 9], "unflushed zeros hide the base");
    assert_eq!(after_loss(&store), [9, 9, 9]);
    store.flush();
    assert_eq!(after_loss(&store), [0, 0, 9], "durable zeros hide the base");
    store.write(0, &fill(7, 1)).unwrap();
    store.flush();
    store.write(0, &fill(0, 1)).unwrap();
    store.write(1, &fill(5, 1)).unwrap();
    assert_eq!(all(&store), [0, 5, 9], "unflushed zeros hide durable data");
    store.discard_unflushed();
    assert_eq!(
        all(&store),
        [7, 0, 9],
        "loss restores durable data, not base"
    );
}

#[test]
fn failed_operations_change_nothing() {
    let mut store = Store::from_image(fill(1, 4)).unwrap();
    store.write(0, &fill(2, 1)).unwrap();
    store.flush();
    store.write(1, &fill(3, 1)).unwrap();
    let (visible, durable) = (all(&store), after_loss(&store));
    for (sector, length, error) in [
        (3, 2 * SECTOR, Error::Range), // starts in range, ends past it
        (4, SECTOR, Error::Range),
        (u64::MAX, SECTOR, Error::Range),
        (0, 100, Error::Alignment),
        (0, SECTOR + 1, Error::Alignment),
        (5, 1, Error::Alignment),
    ] {
        assert_eq!(store.write(sector, &vec![8; length]), Err(error.clone()));
        let mut buffer = vec![0xa5; length];
        assert_eq!(store.read(sector, &mut buffer), Err(error));
        assert!(buffer.iter().all(|&b| b == 0xa5), "failed read wrote");
    }
    assert_eq!((all(&store), after_loss(&store)), (visible, durable));
    // An empty transfer within the end of the store is a no-op.
    store.write(4, &[]).unwrap();
    store.read(4, &mut []).unwrap();
    assert_eq!(store.write(5, &[]), Err(Error::Range));
}

#[test]
fn captures_preserve_both_layers_without_promoting() {
    let mut source = Store::from_image(fill(1, 3)).unwrap();
    source.write(0, &fill(2, 1)).unwrap();
    source.flush();
    source.write(1, &fill(3, 1)).unwrap();
    let capture = source.clone();
    // Capture neither flushed the source nor lost the unflushed write.
    assert_eq!(
        (all(&source), after_loss(&source)),
        ([2, 3, 1].into(), [2, 1, 1].into())
    );
    assert_eq!(
        (all(&capture), after_loss(&capture)),
        ([2, 3, 1].into(), [2, 1, 1].into())
    );
    // A restored copy may still flush the captured write to make it durable.
    let mut restored = capture.clone();
    restored.flush();
    assert_eq!(after_loss(&restored), [2, 3, 1]);
    assert_eq!(after_loss(&capture), [2, 1, 1]);
}

#[test]
fn captures_and_sources_are_independent() {
    let mut source = Store::zeroed(4).unwrap();
    source.write(0, &fill(1, 2)).unwrap();
    source.flush();
    source.write(1, &fill(2, 1)).unwrap();
    let capture = source.clone();
    let expected = (all(&capture), after_loss(&capture));
    // Every kind of later source change leaves the capture unchanged.
    source.write(0, &fill(5, 4)).unwrap();
    assert_eq!((all(&capture), after_loss(&capture)), expected);
    source.flush();
    source.write(2, &fill(6, 1)).unwrap();
    source.discard_unflushed();
    assert_eq!((all(&capture), after_loss(&capture)), expected);
    drop(source);
    assert_eq!((all(&capture), after_loss(&capture)), expected);
    // And restoring then changing a copy leaves the capture unchanged.
    let mut restored = capture.clone();
    let mut continued = capture.clone();
    restored.discard_unflushed();
    continued.write(3, &fill(7, 1)).unwrap();
    continued.flush();
    assert_eq!((all(&capture), after_loss(&capture)), expected);
    assert_eq!(all(&restored), [1, 1, 0, 0]);
    assert_eq!(after_loss(&continued), [1, 2, 0, 7]);
}

#[test]
fn repeated_losses_keep_durable_data_and_lose_only_new_writes() {
    let mut store = Store::from_image(fill(b'.', 8)).unwrap();
    for round in 0..3u8 {
        store.write(round.into(), &fill(b'a' + round, 1)).unwrap();
        store.flush();
        store.write(7, &fill(b'x', 1)).unwrap();
        store.discard_unflushed();
    }
    assert_eq!(all(&store), b"abc.....");
}

// Exhaustive crash-oracle histories over three sectors with nonzero and zero
// base data, run against the store and against deliberately broken models.

#[derive(Clone, Copy, Debug)]
enum Op {
    Write(u64, u8),
    Flush,
    Discard,
}

/// Independent reference: full-array current and durable views.
struct Oracle {
    current: Vec<u8>,
    durable: Vec<u8>,
}

const BASE: [u8; 3] = [5, 6, 0];

/// The model under test, through operations only; `capture` may mutate a
/// broken source to model a promoting snapshot.
trait Model: Sized {
    fn new() -> Self;
    fn apply(&mut self, op: Op);
    fn capture(&mut self) -> Self;
    fn read(&self) -> Vec<u8>;
    fn discard(&mut self);
}

impl Model for Store {
    fn new() -> Self {
        Store::from_image(BASE.iter().flat_map(|&b| fill(b, 1)).collect::<Vec<u8>>()).unwrap()
    }
    fn apply(&mut self, op: Op) {
        match op {
            Op::Write(sector, value) => self.write(sector, &fill(value, 1)).unwrap(),
            Op::Flush => self.flush(),
            Op::Discard => self.discard_unflushed(),
        }
    }
    fn capture(&mut self) -> Self {
        self.clone()
    }
    fn read(&self) -> Vec<u8> {
        all(self)
    }
    fn discard(&mut self) {
        self.discard_unflushed()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Broken {
    EarlyFlushAck,
    Writethrough,
    DurableErasure,
    FlushLatestOnly,
    ZeroAsHole,
    ReadDurableOnly,
    PromotingCapture,
    DiscardKeepsLatest,
}

#[derive(Clone)]
struct Mutant {
    kind: Broken,
    store: Store,
    latest: Option<u64>,
}

thread_local!(static KIND: std::cell::Cell<Broken> = const { std::cell::Cell::new(Broken::EarlyFlushAck) });

impl Model for Mutant {
    fn new() -> Self {
        Self {
            kind: KIND.get(),
            store: Store::new(),
            latest: None,
        }
    }
    fn apply(&mut self, op: Op) {
        let store = &mut self.store;
        match (op, self.kind) {
            (Op::Write(sector, 0), Broken::ZeroAsHole) => {
                store.unflushed.remove(&sector);
            }
            (Op::Write(sector, _), _) => {
                self.latest = Some(sector);
                store.apply(op);
            }
            (Op::Flush, Broken::EarlyFlushAck) => {}
            (Op::Flush, Broken::FlushLatestOnly) => {
                let latest = self
                    .latest
                    .and_then(|s| store.unflushed.remove(&s).map(|d| (s, d)));
                if let Some((sector, data)) = latest {
                    store.durable.insert(sector, data);
                }
            }
            (Op::Discard, _) => self.discard(),
            (Op::Flush, _) => store.flush(),
        }
    }
    fn capture(&mut self) -> Self {
        if self.kind == Broken::PromotingCapture {
            self.store.flush();
        }
        self.clone()
    }
    fn read(&self) -> Vec<u8> {
        if self.kind == Broken::ReadDurableOnly {
            let mut durable = self.store.clone();
            durable.discard_unflushed();
            return all(&durable);
        }
        all(&self.store)
    }
    fn discard(&mut self) {
        match self.kind {
            Broken::Writethrough => {}
            Broken::DurableErasure => {
                self.store.discard_unflushed();
                self.store.durable.clear();
            }
            Broken::DiscardKeepsLatest => {
                let keep = self
                    .latest
                    .and_then(|s| self.store.unflushed.remove(&s).map(|d| (s, d)));
                self.store.discard_unflushed();
                if let Some((sector, data)) = keep {
                    self.store.durable.insert(sector, data);
                }
            }
            _ => self.store.discard_unflushed(),
        }
    }
}

fn operations() -> Vec<Op> {
    let mut ops = vec![Op::Flush, Op::Discard];
    for sector in 0..BASE.len() as u64 {
        for value in [0, 7, 9] {
            ops.push(Op::Write(sector, value));
        }
    }
    ops
}

/// Check one history; after every step, the model and a capture taken then
/// must match the oracle's current and durable views.
fn conforms<M: Model>(history: &[Op]) -> bool {
    let mut model = M::new();
    let mut oracle = Oracle {
        current: BASE.to_vec(),
        durable: BASE.to_vec(),
    };
    for &op in history {
        model.apply(op);
        match op {
            Op::Write(sector, value) => oracle.current[sector as usize] = value,
            Op::Flush => oracle.durable = oracle.current.clone(),
            Op::Discard => oracle.current = oracle.durable.clone(),
        }
        let mut capture = model.capture();
        let visible = capture.read();
        capture.discard();
        if model.read() != oracle.current
            || visible != oracle.current
            || capture.read() != oracle.durable
        {
            return false;
        }
    }
    true
}

fn histories(length: usize) -> impl Iterator<Item = Vec<Op>> {
    let ops = operations();
    let count = ops.len().pow(length as u32);
    (0..count).map(move |mut index| {
        (0..length)
            .map(|_| {
                let op = ops[index % ops.len()];
                index /= ops.len();
                op
            })
            .collect()
    })
}

#[test]
fn every_short_history_matches_the_crash_oracle() {
    let mut checked = 0;
    for length in 0..=4 {
        for history in histories(length) {
            assert!(conforms::<Store>(&history), "{history:?}");
            checked += 1;
        }
    }
    assert_eq!(checked, 1 + 11 + 121 + 1331 + 14641);
}

#[test]
fn the_oracle_detects_each_broken_model() {
    for kind in [
        Broken::EarlyFlushAck,
        Broken::Writethrough,
        Broken::DurableErasure,
        Broken::FlushLatestOnly,
        Broken::ZeroAsHole,
        Broken::ReadDurableOnly,
        Broken::PromotingCapture,
        Broken::DiscardKeepsLatest,
    ] {
        KIND.set(kind);
        let failing = (0..=4)
            .flat_map(histories)
            .find(|history| !conforms::<Mutant>(history));
        assert!(failing.is_some(), "{kind:?} went undetected");
    }
}

#[test]
fn crash_selection_visits_latest_sectors_in_order_and_keeps_source_independent() {
    let mut source = Store::zeroed(8).unwrap();
    source.write(2, &fill(3, 1)).unwrap();
    source.flush();
    source.write(5, &fill(6, 1)).unwrap();
    source.write(2, &fill(4, 1)).unwrap();
    source.write(5, &fill(7, 1)).unwrap();
    source.write(1, &fill(0, 1)).unwrap();
    let mut visited = Vec::new();
    let (mut next, lost) = source.crashed_keeping(|sector, bytes| {
        visited.push((sector, bytes[0]));
        sector == 5
    });
    assert_eq!(visited, [(1, 0), (2, 4), (5, 7)]);
    assert_eq!(lost, 2);
    assert_eq!(all(&source), [0, 0, 4, 0, 0, 7, 0, 0]);
    assert_eq!(all(&next), [0, 0, 3, 0, 0, 7, 0, 0]);
    next.discard_unflushed();
    assert_eq!(at(&next, 5), 7, "selected sector is durable");
    next.write(5, &fill(8, 1)).unwrap();
    assert_eq!(at(&source, 5), 7);
}

#[test]
fn crash_selection_distinguishes_equal_overwrites_and_zero_survival_matches_default() {
    let mut source = Store::zeroed(2).unwrap();
    source.write(0, &fill(1, 1)).unwrap();
    source.flush();
    source.write(0, &fill(1, 1)).unwrap();
    source.write(1, &fill(0, 1)).unwrap();
    let mut count = 0;
    let (next, lost) = source.crashed_keeping(|_, _| {
        count += 1;
        false
    });
    assert_eq!((count, lost), (2, 2));
    assert_eq!(all(&next), after_loss(&source));
    let (next, lost) = source.crashed_keeping(|_, _| true);
    assert_eq!(lost, 0);
    assert_eq!(all(&next), all(&source));
}
