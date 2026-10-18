use super::*;

fn write(kind: fn(LifecycleWrite) -> GuestWrite, time: u64, bytes: &[u8]) -> GuestWrite {
    kind(LifecycleWrite {
        time,
        bytes: bytes.to_vec(),
    })
}

fn frame(length: usize, tag: u8) -> Vec<u8> {
    (0..length).map(|i| tag ^ i as u8).collect()
}

fn encoded_writes(writes: &[GuestWrite]) -> Vec<u8> {
    let mut bytes = Vec::new();
    send_writes(&mut bytes, writes).unwrap();
    bytes
}

#[test]
fn equal_time_writes_keep_their_interleaving_and_the_stream_continues() {
    let writes = vec![
        write(GuestWrite::Console, 5, b"a"),
        write(GuestWrite::Frame, 5, &frame(60, 1)),
        write(GuestWrite::Lifecycle, 5, b"ready\n"),
        write(GuestWrite::Frame, 5, &frame(14, 2)),
        write(GuestWrite::Console, 5, b""),
        write(GuestWrite::Frame, 9, &frame(1514, 3)),
        write(GuestWrite::Lifecycle, 9, &[0; 65536]),
    ];
    let mut bytes = encoded_writes(&writes);
    bytes.extend(b"next message");
    let mut input = &bytes[..];
    assert_eq!(receive_writes(&mut input, 7).unwrap(), writes);
    assert_eq!(input, b"next message");
    assert!(receive_writes(&mut &encoded_writes(&[])[..], 0)
        .unwrap()
        .is_empty());
}

#[test]
fn write_records_beyond_the_json_cap_round_trip() {
    // 1.2 million one-byte console writes: about 70 MiB as JSON markers.
    let mut writes: Vec<_> = (0..1_200_000u64)
        .map(|i| write(GuestWrite::Console, i / 3, &[i as u8]))
        .collect();
    // Plus about 15 MiB of frames.
    writes.extend((0..10_000u64).map(|i| write(GuestWrite::Frame, i, &frame(1514, i as u8))));
    let json = serde_json::to_vec(&writes).unwrap();
    assert!(json.len() > 8 << 20);
    let bytes = encoded_writes(&writes);
    let count = writes.len() as u64;
    assert_eq!(receive_writes(&mut &bytes[..], count).unwrap(), writes);
}

#[test]
fn malformed_write_trailers_are_errors() {
    let writes = [
        write(GuestWrite::Lifecycle, 1, b"abc"),
        write(GuestWrite::Frame, 2, &frame(20, 0)),
    ];
    let bytes = encoded_writes(&writes);
    // Every truncation fails, including a missing end tag.
    for end in 0..bytes.len() {
        assert!(receive_writes(&mut &bytes[..end], 2).is_err(), "{end}");
    }
    // Count mismatch in either direction.
    assert!(receive_writes(&mut &bytes[..], 1).is_err());
    assert!(receive_writes(&mut &bytes[..], 3).is_err());
    // Unknown tag.
    let mut bad = bytes.clone();
    bad[0] = 7;
    assert!(receive_writes(&mut &bad[..], 2).is_err());
    // A frame record with an out-of-bounds length fails before its body.
    let at = 1 + 8 + 4 + 3;
    for length in [0u32, 13, 1515, u32::MAX] {
        let mut bad = bytes.clone();
        bad[at + 9..at + 13].copy_from_slice(&length.to_le_bytes());
        assert!(receive_writes(&mut &bad[..], 2).is_err(), "{length}");
    }
    // A huge declared console length without bytes allocates nothing large.
    let mut huge = vec![CONSOLE];
    huge.extend(0u64.to_le_bytes());
    huge.extend(u32::MAX.to_le_bytes());
    huge.extend(b"short");
    assert!(receive_writes(&mut &huge[..], 1).is_err());
    // Frames outside the bounds are never sent.
    for length in [13, 1515] {
        let invalid = [write(GuestWrite::Frame, 0, &frame(length, 0))];
        assert!(send_writes(&mut Vec::new(), &invalid).is_err());
    }
}

/// A live pipe: these bytes, then a peer that sends nothing more until
/// it receives a reply. Reading past them would block forever.
struct Live<'a>(&'a [u8]);
impl Read for Live<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        assert!(!self.0.is_empty(), "read past the bytes a live peer sent");
        let count = buf.len().min(self.0.len());
        buf[..count].copy_from_slice(&self.0[..count]);
        self.0 = &self.0[count..];
        Ok(count)
    }
}

#[test]
fn malformed_trailers_fail_without_reading_past_the_peer() {
    // An early end tag, an unknown tag, or an out-of-bounds frame length
    // fails as soon as it is read.
    assert!(receive_writes(&mut Live(&[END]), 1).is_err());
    assert!(receive_writes(&mut Live(&[7]), 1).is_err());
    let mut header = vec![FRAME];
    header.extend(5u64.to_le_bytes());
    header.extend(13u32.to_le_bytes());
    assert!(receive_writes(&mut Live(&header), 1).is_err());
    // A whole valid trailer, read to its end tag and no further.
    let writes = [write(GuestWrite::Lifecycle, 1, b"abc")];
    let bytes = encoded_writes(&writes);
    assert_eq!(receive_writes(&mut Live(&bytes), 1).unwrap(), writes);
    assert!(receive_writes(&mut Live(&bytes), 2).is_err());
    // Frames: an early end mark or a bad length, then nothing more.
    assert!(receive_frames(&mut Live(&[0, 0]), 1).is_err());
    assert!(receive_frames(&mut Live(&13u16.to_le_bytes()), 1).is_err());
    let mut bytes = Vec::new();
    send_frames(&mut bytes, &[frame(14, 1)]).unwrap();
    assert_eq!(
        receive_frames(&mut Live(&bytes), 1).unwrap(),
        [frame(14, 1)]
    );
}

#[test]
fn host_frames_round_trip_in_order_and_reject_malformed_trailers() {
    let frames: Vec<_> = (0..12_000).map(|i| frame(14 + i % 1501, i as u8)).collect();
    let mut bytes = Vec::new();
    send_frames(&mut bytes, &frames).unwrap();
    assert!(bytes.len() > 8 << 20);
    bytes.extend(b"tail");
    let mut input = &bytes[..];
    assert_eq!(receive_frames(&mut input, 12_000).unwrap(), frames);
    assert_eq!(input, b"tail");

    let mut bytes = Vec::new();
    send_frames(&mut bytes, &[frame(14, 1), frame(1514, 2)]).unwrap();
    for end in 0..bytes.len() {
        assert!(receive_frames(&mut &bytes[..end], 2).is_err(), "{end}");
    }
    assert!(receive_frames(&mut &bytes[..], 1).is_err());
    assert!(receive_frames(&mut &bytes[..], 3).is_err());
    for length in [13u16, 1515, u16::MAX] {
        let mut bad = bytes.clone();
        bad[..2].copy_from_slice(&length.to_le_bytes());
        assert!(receive_frames(&mut &bad[..], 2).is_err(), "{length}");
    }
    for length in [0, 13, 1515] {
        assert!(send_frames(&mut Vec::new(), &[frame(length, 0)]).is_err());
    }
    let mut empty = Vec::new();
    send_frames(&mut empty, &[]).unwrap();
    assert!(receive_frames(&mut &empty[..], 0).unwrap().is_empty());
}
