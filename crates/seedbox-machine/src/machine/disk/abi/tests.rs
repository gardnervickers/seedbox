use super::*;

fn iov(bytes: &mut [u8]) -> SeedboxIov {
    SeedboxIov {
        base: bytes.as_mut_ptr(),
        len: bytes.len(),
    }
}

fn header(kind: u32, sector: u64) -> Vec<u8> {
    let mut bytes = kind.to_le_bytes().to_vec();
    bytes.extend([0; 4]);
    bytes.extend(sector.to_le_bytes());
    bytes
}

/// Run one request through the C entry point.
unsafe fn execute(
    disk: *mut SeedboxDisk,
    out: &[SeedboxIov],
    in_: &[SeedboxIov],
) -> Option<(u32, u8)> {
    let (mut written, mut status) = (0, 0xff);
    seedbox_disk_execute(
        disk,
        out.as_ptr(),
        out.len(),
        in_.as_ptr(),
        in_.len(),
        &mut written,
        &mut status,
    )
    .then_some((written, status))
}

#[test]
fn open_validates_sizes_and_copies_the_image() {
    unsafe {
        assert!(seedbox_disk_open(0, ptr::null(), 0).is_null());
        assert!(seedbox_disk_open(1, ptr::null(), 512).is_null());
        let mut image = vec![7u8; 2 * SECTOR];
        assert!(seedbox_disk_open(1, image.as_ptr(), image.len()).is_null());
        assert!(seedbox_disk_open(u64::MAX, image.as_ptr(), image.len()).is_null());
        let disk = seedbox_disk_open(2, image.as_ptr(), image.len());
        assert!(!disk.is_null());
        image.fill(0); // the store owns its copy
        assert_eq!(seedbox_disk_capacity(disk), 2);
        let mut data = vec![0; SECTOR];
        let mut status = [0xffu8];
        let mut head = header(0, 1);
        let result = execute(disk, &[iov(&mut head)], &[iov(&mut data), iov(&mut status)]);
        assert_eq!(result, Some((SECTOR as u32 + 1, 0)));
        assert!(data.iter().all(|&b| b == 7) && status == [0]);
        seedbox_disk_close(disk);
        seedbox_disk_close(ptr::null_mut());
        assert_eq!(seedbox_disk_capacity(ptr::null()), 0);
        let zeroed = seedbox_disk_open(1 << 40, ptr::null(), 0);
        assert_eq!(seedbox_disk_capacity(zeroed), 1 << 40);
        seedbox_disk_close(zeroed);
    }
}

#[test]
fn split_and_overlapping_segments_follow_stream_order() {
    unsafe {
        let disk = seedbox_disk_open(4, ptr::null(), 0);
        // A write whose header and sector data split across odd segments.
        let mut request = [header(1, 2), vec![b'W'; SECTOR]].concat();
        let (first, rest) = request.split_at_mut(7);
        let (middle, last) = rest.split_at_mut(300);
        let mut status = [0xffu8];
        let out = [iov(first), iov(middle), iov(last)];
        assert_eq!(execute(disk, &out, &[iov(&mut status)]), Some((1, 0)));
        // A read whose status segment aliases the last data byte.
        let mut head = header(0, 2);
        let mut data = vec![0u8; SECTOR];
        let alias = SeedboxIov {
            base: data.as_mut_ptr().add(SECTOR - 1),
            len: 1,
        };
        let in_ = [iov(&mut data), alias];
        assert_eq!(
            execute(disk, &[iov(&mut head)], &in_),
            Some((SECTOR as u32 + 1, 0))
        );
        assert!(data[..SECTOR - 1].iter().all(|&b| b == b'W'));
        assert_eq!(data[SECTOR - 1], 0, "status written after data");
        seedbox_disk_close(disk);
    }
}

#[test]
fn malformed_requests_and_invalid_arguments_have_no_effect() {
    unsafe {
        let disk = seedbox_disk_open(2, ptr::null(), 0);
        let before = (*disk).0.encode_overlays();
        let mut write = [header(1, 0), vec![b'M'; SECTOR]].concat();
        let mut status = [0xccu8];
        // No status byte; header too short.
        assert_eq!(execute(disk, &[iov(&mut write)], &[]), None);
        assert_eq!(
            execute(disk, &[iov(&mut write[..15])], &[iov(&mut status)]),
            None
        );
        // Null segment base, too many segments, null array or outputs.
        let null = SeedboxIov {
            base: ptr::null_mut(),
            len: 1,
        };
        assert_eq!(
            execute(disk, &[iov(&mut write), null], &[iov(&mut status)]),
            None
        );
        let many: Vec<_> = (0..=MAX_SEGMENTS).map(|_| iov(&mut [])).collect();
        assert_eq!(execute(disk, &many, &[iov(&mut status)]), None);
        let mut written = 0;
        assert!(!seedbox_disk_execute(
            disk,
            ptr::null(),
            1,
            ptr::null(),
            0,
            &mut written,
            &mut 0
        ));
        let out = [iov(&mut write)];
        let in_ = [iov(&mut status)];
        assert!(!seedbox_disk_execute(
            disk,
            out.as_ptr(),
            1,
            in_.as_ptr(),
            1,
            ptr::null_mut(),
            &mut 0
        ));
        assert!(!seedbox_disk_execute(
            ptr::null_mut(),
            out.as_ptr(),
            1,
            in_.as_ptr(),
            1,
            &mut written,
            &mut 0
        ));
        assert_eq!(status, [0xcc]);
        assert_eq!((*disk).0.encode_overlays(), before);
        seedbox_disk_close(disk);
    }
}

#[test]
fn overlays_encode_and_replace_without_promoting() {
    unsafe {
        let disk = seedbox_disk_open(2, ptr::null(), 0);
        let store = &mut (*disk).0;
        store.write(0, &[1; SECTOR]).unwrap();
        store.flush();
        store.write(1, &[2; SECTOR]).unwrap();
        let (mut bytes, mut len) = (ptr::null_mut(), 0);
        assert!(seedbox_disk_encode(disk, &mut bytes, &mut len));
        let encoded = std::slice::from_raw_parts(bytes, len).to_vec();
        assert_eq!(encoded, (*disk).0.encode_overlays());
        let other = seedbox_disk_open(2, ptr::null(), 0);
        assert!(seedbox_disk_replace_overlays(other, bytes, len));
        let mut lost = (*other).0.clone();
        lost.discard_unflushed();
        assert_eq!(
            (
                (*other).0.sector(1)[0],
                lost.sector(1)[0],
                lost.sector(0)[0]
            ),
            (2, 0, 1)
        );
        // Invalid input leaves the destination unchanged.
        assert!(!seedbox_disk_replace_overlays(other, bytes, len - 1));
        assert!(!seedbox_disk_replace_overlays(other, ptr::null(), 3));
        assert!(!seedbox_disk_replace_overlays(other, ptr::null(), 0));
        assert!(!seedbox_disk_encode(ptr::null(), &mut bytes, &mut len));
        assert_eq!((*other).0.encode_overlays(), encoded);
        seedbox_disk_bytes_free(bytes, len);
        seedbox_disk_bytes_free(ptr::null_mut(), 0);
        seedbox_disk_close(disk);
        seedbox_disk_close(other);
    }
}

/// A test source: serves `bytes`, records requested lengths.
struct Source {
    bytes: Vec<u8>,
    offset: usize,
    requests: Vec<usize>,
}

unsafe extern "C" fn source_read(context: *mut c_void, buffer: *mut u8, length: usize) -> usize {
    let source = &mut *(context as *mut Source);
    source.requests.push(length);
    let count = length.min(source.bytes.len() - source.offset);
    ptr::copy_nonoverlapping(source.bytes[source.offset..].as_ptr(), buffer, count);
    source.offset += count;
    count
}

unsafe fn replace_from(disk: *mut SeedboxDisk, length: u64, bytes: Vec<u8>) -> (bool, Vec<usize>) {
    let mut source = Source {
        bytes,
        offset: 0,
        requests: Vec::new(),
    };
    let context = (&mut source as *mut Source).cast();
    let replaced = seedbox_disk_replace_overlays_from(disk, length, Some(source_read), context);
    (replaced, source.requests)
}

#[test]
fn streamed_overlays_restore_in_bounded_chunks() {
    unsafe {
        // 300 sectors in both layers: an encoding larger than one chunk.
        let source = seedbox_disk_open(300, ptr::null(), 0);
        for sector in 0..300 {
            (*source).0.write(sector, &[sector as u8; SECTOR]).unwrap();
        }
        (*source).0.flush();
        (*source).0.write(7, &[1; SECTOR]).unwrap();
        let encoded = (*source).0.encode_overlays();
        assert!(encoded.len() > 1 << 16);
        let disk = seedbox_disk_open(300, ptr::null(), 0);
        let (replaced, requests) = replace_from(disk, encoded.len() as u64, encoded.clone());
        assert!(replaced);
        assert!(requests.iter().all(|&length| length <= 1 << 16));
        assert_eq!(requests.iter().sum::<usize>(), encoded.len());
        assert_eq!((*disk).0.encode_overlays(), encoded);
        seedbox_disk_close(source);
        seedbox_disk_close(disk);
    }
}

#[test]
fn streamed_overlays_reject_bad_lengths_without_change() {
    unsafe {
        let disk = seedbox_disk_open(2, ptr::null(), 0);
        (*disk).0.write(1, &[3; SECTOR]).unwrap();
        let before = (*disk).0.encode_overlays();
        let limit = 32 + 2 * 2 * (8 + SECTOR as u64);
        // Lengths beyond any valid encoding are refused before reading,
        // including lengths not representable in memory.
        for length in [limit + 1, u64::MAX] {
            let (replaced, requests) = replace_from(disk, length, before.clone());
            assert!(!replaced && requests.is_empty(), "{length}");
        }
        // A declared length longer than the stream is truncated.
        let short = before.len() as u64 + 8;
        let (replaced, requests) = replace_from(disk, short, before.clone());
        assert!(!replaced);
        assert_eq!(requests, [short as usize]);
        // A declared length shorter than the encoding is invalid.
        let (replaced, _) = replace_from(disk, before.len() as u64 - 1, before.clone());
        assert!(!replaced);
        // Missing callback or handle.
        let context = ptr::null_mut();
        assert!(!seedbox_disk_replace_overlays_from(disk, 0, None, context));
        assert!(!seedbox_disk_replace_overlays_from(
            ptr::null_mut(),
            0,
            Some(source_read),
            context
        ));
        assert_eq!((*disk).0.encode_overlays(), before);
        // A maximal valid length for this capacity is accepted.
        let full = Store::zeroed(2).unwrap();
        let mut full = full;
        full.write(0, &[1; 2 * SECTOR]).unwrap();
        full.flush();
        full.write(0, &[2; 2 * SECTOR]).unwrap();
        let encoded = full.encode_overlays();
        assert_eq!(encoded.len() as u64, limit);
        assert!(replace_from(disk, limit, encoded).0);
        seedbox_disk_close(disk);
    }
}

#[test]
fn streamed_lengths_past_memory_or_u32_are_handled_without_huge_allocations() {
    unsafe {
        // A sparse disk whose encoding limit exceeds every usable length.
        let disk = seedbox_disk_open(u64::MAX, ptr::null(), 0);
        let before = (*disk).0.encode_overlays();
        for length in [isize::MAX as u64 + 1, u64::MAX] {
            let (replaced, requests) = replace_from(disk, length, vec![]);
            assert!(!replaced && requests.is_empty(), "{length}");
        }
        // Past u32::MAX is not rejected early: one bounded chunk is requested,
        // the stream ends, and the store is unchanged.
        let length = u32::MAX as u64 + 1;
        let (replaced, requests) = replace_from(disk, length, vec![]);
        assert!(!replaced);
        assert_eq!(requests, [1 << 16]);
        assert_eq!((*disk).0.encode_overlays(), before);
        seedbox_disk_close(disk);
    }
}
