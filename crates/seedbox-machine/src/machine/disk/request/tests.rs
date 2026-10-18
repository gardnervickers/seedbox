use super::*;

/// Contiguous gathered streams; the writable stream starts filled with 0xcc.
struct Gathered {
    readable: Vec<u8>,
    writable: Vec<u8>,
}

impl Buffers for Gathered {
    fn readable(&self) -> usize {
        self.readable.len()
    }
    fn writable(&self) -> usize {
        self.writable.len()
    }
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.readable[offset..offset + out.len()]);
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) {
        self.writable[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Outcome {
    data: Vec<u8>,
    status: u8,
}

impl Outcome {
    fn written(&self) -> usize {
        self.data.len() + 1
    }
}

/// Run one request over gathered streams and check its writable effects:
/// written data, untouched bytes up to the status, and the status byte.
fn execute(store: &mut Store, readable: &[u8], writable: usize) -> Result<Outcome, Malformed> {
    let mut buffers = Gathered {
        readable: readable.to_vec(),
        writable: vec![0xcc; writable],
    };
    let completion = super::execute(store, &mut buffers)?;
    let written = completion.written as usize;
    let (data, rest) = buffers.writable.split_at(written - 1);
    assert!(rest[..rest.len() - 1].iter().all(|&b| b == 0xcc));
    assert_eq!(rest.last(), Some(&completion.status));
    Ok(Outcome {
        data: data.to_vec(),
        status: completion.status,
    })
}

fn header(kind: u32, sector: u64) -> Vec<u8> {
    let mut bytes = kind.to_le_bytes().to_vec();
    bytes.extend([0xee; 4]); // reserved; ignored
    bytes.extend(sector.to_le_bytes());
    bytes
}

fn with(kind: u32, sector: u64, data: &[u8]) -> Vec<u8> {
    [header(kind, sector), data.to_vec()].concat()
}

/// Read back every sector's first byte and what survives a power loss.
fn view(store: &Store) -> (Vec<u8>, Vec<u8>) {
    let first = |store: &Store| {
        (0..store.capacity())
            .map(|s| {
                let mut b = [0; SECTOR];
                store.read(s, &mut b).unwrap();
                b[0]
            })
            .collect()
    };
    let mut lost = store.clone();
    lost.discard_unflushed();
    (first(store), first(&lost))
}

fn ok(data: Vec<u8>) -> Outcome {
    Outcome {
        data,
        status: STATUS_OK,
    }
}

fn status(status: u8) -> Outcome {
    Outcome {
        data: vec![],
        status,
    }
}

#[test]
fn reads_writes_and_flushes_follow_store_semantics() {
    let mut store = Store::from_image(vec![1; 4 * SECTOR]).unwrap();
    let out = |b: u8, n: usize| with(TYPE_OUT, 1, &vec![b; n * SECTOR]);
    assert_eq!(execute(&mut store, &out(2, 2), 1), Ok(ok(vec![])));
    // A read sees the unflushed write; its data fills the writable room.
    let read = execute(&mut store, &header(TYPE_IN, 0), 3 * SECTOR + 1).unwrap();
    assert_eq!((read.status, read.written()), (STATUS_OK, 3 * SECTOR + 1));
    let firsts: Vec<u8> = read.data.chunks(SECTOR).map(|s| s[0]).collect();
    assert_eq!(firsts, [1, 2, 2]);
    assert_eq!(view(&store), (vec![1, 2, 2, 1], vec![1, 1, 1, 1]));
    assert_eq!(
        execute(&mut store, &header(TYPE_FLUSH, 0), 1),
        Ok(ok(vec![]))
    );
    execute(&mut store, &out(3, 1), 1).unwrap();
    assert_eq!(view(&store), (vec![1, 3, 2, 1], vec![1, 2, 2, 1]));
    // An empty read or write is a completed no-op.
    assert_eq!(execute(&mut store, &header(TYPE_IN, 4), 1), Ok(ok(vec![])));
    assert_eq!(execute(&mut store, &header(TYPE_OUT, 4), 1), Ok(ok(vec![])));
}

#[test]
fn get_id_fills_at_most_twenty_bytes() {
    let mut store = Store::zeroed(1).unwrap();
    let id = execute(&mut store, &header(TYPE_GET_ID, 0), 21).unwrap();
    assert_eq!(id, ok(ID.to_vec()));
    let short = execute(&mut store, &header(TYPE_GET_ID, 0), 5).unwrap();
    assert_eq!(short, ok(b"seed".to_vec()));
    let long = execute(&mut store, &header(TYPE_GET_ID, 0), 64).unwrap();
    assert_eq!((long.data.len(), long.written()), (20, 21));
}

#[test]
fn guest_errors_complete_without_any_effect() {
    let mut store = Store::from_image(vec![1; 4 * SECTOR]).unwrap();
    execute(&mut store, &with(TYPE_OUT, 0, &[5; SECTOR]), 1).unwrap();
    let before = view(&store);
    let ioerr = status(STATUS_IOERR);
    for (readable, writable) in [
        // Past the end, including partially in range and sector overflow.
        (with(TYPE_OUT, 3, &[9; 2 * SECTOR]), 1),
        (with(TYPE_OUT, u64::MAX, &[9; SECTOR]), 1),
        (header(TYPE_IN, 3), 2 * SECTOR + 1),
        (header(TYPE_IN, 5), 1),
        // Partial sectors.
        (with(TYPE_OUT, 0, &[9; 100]), 1),
        (header(TYPE_IN, 0), 101),
        // Data in the wrong direction for the type.
        (with(TYPE_OUT, 0, &[9; SECTOR]), SECTOR + 1),
        (with(TYPE_IN, 0, &[9; SECTOR]), SECTOR + 1),
        (with(TYPE_FLUSH, 0, &[9; SECTOR]), 1),
        (header(TYPE_FLUSH, 0), SECTOR + 1),
        (with(TYPE_GET_ID, 0, &[9; 4]), 21),
    ] {
        assert_eq!(execute(&mut store, &readable, writable), Ok(ioerr.clone()));
        assert_eq!(view(&store), before, "{readable:?}");
    }
    // A misplaced flush did not make the earlier write durable.
    assert_eq!(view(&store).1, [1, 1, 1, 1]);
    for kind in [2, 3, 5, 7, 11, 13, u32::MAX] {
        let unsupported = execute(&mut store, &with(kind, 0, &[9; SECTOR]), 9);
        assert_eq!(unsupported, Ok(status(STATUS_UNSUPP)));
        assert_eq!(view(&store), before);
    }
}

#[test]
fn requests_without_header_or_status_are_malformed_and_change_nothing() {
    let mut store = Store::zeroed(2).unwrap();
    execute(&mut store, &with(TYPE_OUT, 0, &[5; SECTOR]), 1).unwrap();
    let before = view(&store);
    for (readable, writable) in [
        (vec![], 1),
        (header(TYPE_OUT, 0)[..15].to_vec(), 1),
        (with(TYPE_OUT, 1, &[7; SECTOR]), 0),
        (header(TYPE_FLUSH, 0), 0),
        (header(TYPE_IN, 0), 0),
    ] {
        assert_eq!(execute(&mut store, &readable, writable), Err(Malformed));
        assert_eq!(view(&store), before);
    }
}

/// Streams of any length with no backing memory: reads return zeros and
/// writes are only counted, so huge requests prove bounded execution.
struct Sized {
    header: Vec<u8>,
    readable: usize,
    writable: usize,
    largest: usize,
    written: usize,
    status: Option<u8>,
}

impl Buffers for Sized {
    fn readable(&self) -> usize {
        self.readable
    }
    fn writable(&self) -> usize {
        self.writable
    }
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        assert!(offset + out.len() <= self.readable);
        self.largest = self.largest.max(out.len());
        out.fill(0);
        let header = self.header.len().saturating_sub(offset).min(out.len());
        if header > 0 {
            out[..header].copy_from_slice(&self.header[offset..offset + header]);
        }
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= self.writable);
        self.largest = self.largest.max(bytes.len());
        if offset + 1 == self.writable && bytes.len() == 1 {
            self.status = Some(bytes[0]);
        } else {
            self.written += bytes.len();
        }
    }
}

fn sized(kind: u32, sector: u64, readable: usize, writable: usize) -> Sized {
    Sized {
        header: header(kind, sector),
        readable,
        writable,
        largest: 0,
        written: 0,
        status: None,
    }
}

#[test]
fn invalid_requests_fail_before_touching_their_data() {
    let mut store = Store::zeroed(4).unwrap();
    for mut request in [
        sized(TYPE_IN, 0, 16, u32::MAX as usize - 16), // partial room
        sized(TYPE_IN, 0, 16, (SECTOR << 20) + 1),     // past the end
        sized(TYPE_IN, u64::MAX, 16, SECTOR + 1),
        sized(TYPE_OUT, 3, 16 + 2 * SECTOR, 1),
    ] {
        let completion = super::execute(&mut store, &mut request).unwrap();
        assert_eq!(
            completion,
            Completion {
                written: 1,
                status: STATUS_IOERR
            }
        );
        assert_eq!((request.written, request.largest), (0, 16));
        assert_eq!(request.status, Some(STATUS_IOERR));
    }
}

#[test]
fn large_requests_execute_one_sector_at_a_time() {
    // A 2 GiB read and write: memory stays at one sector per step.
    let length = 1 << 31;
    let mut store = Store::zeroed(1 << 23).unwrap();
    let mut read = sized(TYPE_IN, 0, 16, length + 1);
    let completion = super::execute(&mut store, &mut read).unwrap();
    assert_eq!(
        completion,
        Completion {
            written: length as u32 + 1,
            status: STATUS_OK
        }
    );
    assert_eq!((read.written, read.largest), (length, SECTOR));
    let mut write = sized(TYPE_OUT, 1, 16 + 4 * SECTOR, 1);
    super::execute(&mut store, &mut write).unwrap();
    assert_eq!(write.largest, SECTOR);
}

#[test]
fn chains_past_u32_are_malformed_before_reading_the_header() {
    let mut store = Store::zeroed(1).unwrap();
    for (readable, writable) in [
        (u32::MAX as usize, 1),
        (16, u32::MAX as usize),
        (usize::MAX, 1),
    ] {
        let mut request = sized(TYPE_OUT, 0, readable, writable);
        assert_eq!(super::execute(&mut store, &mut request), Err(Malformed));
        assert_eq!((request.largest, request.status), (0, None));
    }
    let limit = u32::MAX as usize;
    assert!(representable(limit, 0) && representable(limit - 1, 1));
    assert!(!representable(limit, 1) && !representable(usize::MAX, 1));
}
