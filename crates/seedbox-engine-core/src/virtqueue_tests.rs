use super::*;

fn put16(m: &mut [u8], at: usize, n: u16) {
    m[at..at + 2].copy_from_slice(&n.to_le_bytes());
}
fn put32(m: &mut [u8], at: usize, n: u32) {
    m[at..at + 4].copy_from_slice(&n.to_le_bytes());
}
fn put64(m: &mut [u8], at: usize, n: u64) {
    m[at..at + 8].copy_from_slice(&n.to_le_bytes());
}
const AVAIL: usize = 0x1000;
const USED: usize = 0x1100;
const DATA: usize = 0x2000;
fn fixture(base: u64) -> (Queue, Vec<u8>) {
    let q = Queue {
        num: 8,
        ready: true,
        desc: base,
        avail: base + AVAIL as u64,
        used: base + USED as u64,
        ..Queue::default()
    };
    let mut m = vec![0; 0x4000];
    put64(&mut m, 0, base + DATA as u64);
    put32(&mut m, 8, 3);
    put16(&mut m, AVAIL + 2, 1);
    m[DATA..DATA + 3].copy_from_slice(b"abc");
    (q, m)
}
fn policy() -> Policy {
    Policy {
        limit: 65536,
        direction: Direction::Any,
        reject_empty: true,
        diagnostics: Diagnostics::Virtio,
    }
}

#[test]
fn dma_translation_completion_and_dirty_journal_match_at_every_ram_base() {
    for base in [0, 0x4000_0000, 0x1_0000_0000] {
        let (mut q, mut m) = fixture(base);
        let chain = q.validated_chain(&m, base, policy()).unwrap().unwrap();
        assert_eq!(chain.read(&m).unwrap(), b"abc");
        assert_eq!(chain.segments[0].offset, DATA);
        let mut dirty = Vec::new();
        q.complete_chain(&mut m, &chain, 3, |a, n| dirty.push((a, n)));
        assert_eq!(dirty, [(USED + 4, 8), (USED + 2, 2)]);
        assert_eq!(rd32(&m, USED + 4), 0);
        assert_eq!(rd32(&m, USED + 8), 3);
        assert_eq!(rd16(&m, USED + 2), 1);
        assert_eq!((q.last_avail, q.used_idx), (1, 1));
        assert!(q.validated_chain(&m, base, policy()).unwrap().is_none());
    }
}

#[test]
fn ring_cursors_wrap_and_cloned_queues_complete_identically() {
    let (mut q, mut m) = fixture(0);
    q.last_avail = u16::MAX;
    q.used_idx = u16::MAX;
    put16(&mut m, AVAIL + 2, 0);
    put16(&mut m, AVAIL + 4 + 2 * 7, 0);
    let mut restored = q.clone();
    let mut restored_ram = m.clone();
    let chain = q.validated_chain(&m, 0, policy()).unwrap().unwrap();
    let mut dirty = Vec::new();
    q.complete_chain(&mut m, &chain, 0, |a, n| dirty.push((a, n)));
    restored.complete_chain(&mut restored_ram, &chain, 0, |_, _| {});
    assert_eq!(m, restored_ram);
    assert_eq!((q.last_avail, q.used_idx), (0, 0));
    assert_eq!(dirty, [(USED + 4 + 8 * 7, 8), (USED + 2, 2)]);
    assert_eq!(
        serde_json::to_value(&q).unwrap(),
        serde_json::to_value(&restored).unwrap()
    );
}

#[test]
fn device_policies_preserve_error_precedence_and_empty_chain_rules() {
    let (q, mut m) = fixture(0);
    // Console rejects direction before looking at the invalid address; ARM validates DMA first.
    put64(&mut m, 0, u64::MAX);
    put16(&mut m, 12, 2);
    let console = Policy {
        direction: Direction::Uniform(false),
        diagnostics: Diagnostics::Console,
        ..policy()
    };
    assert_eq!(
        q.validated_chain(&m, 0, console).err().unwrap(),
        "unsupported console descriptor flags/direction"
    );
    assert_eq!(
        q.validated_chain(&m, 0, policy()).err().unwrap(),
        "virtio DMA span is outside RAM"
    );
    let (q, mut m) = fixture(0);
    put32(&mut m, 8, 0);
    assert_eq!(
        q.validated_chain(&m, 0, policy()).err().unwrap(),
        "empty virtio descriptor chain"
    );
    let disk = Policy {
        reject_empty: false,
        direction: Direction::ReadThenWrite,
        diagnostics: Diagnostics::Disk,
        ..policy()
    };
    assert_eq!(q.validated_chain(&m, 0, disk).unwrap().unwrap().len, 0);
    // Read-after-write is a block policy; mixed direction remains valid at ARM's queue boundary.
    put32(&mut m, 8, 1);
    put16(&mut m, 12, 3);
    put16(&mut m, 14, 1);
    put64(&mut m, 16, DATA as u64 + 1);
    put32(&mut m, 24, 1);
    assert_eq!(
        q.validated_chain(&m, 0, disk).err().unwrap(),
        "disk descriptor chain reads after it writes"
    );
    assert_eq!(q.validated_chain(&m, 0, policy()).unwrap().unwrap().len, 2);
}

#[test]
fn invalid_rings_and_chains_preserve_ram_and_queue_state() {
    for defect in 0..10 {
        let (mut q, mut m) = fixture(0x4000_0000);
        match defect {
            0 => q.num = 3,
            1 => q.desc += 1,
            2 => q.avail = u64::MAX - 1,
            3 => q.used = 0x4000_3ffc,
            4 => put16(&mut m, AVAIL + 2, 9),
            5 => put16(&mut m, AVAIL + 4, 8),
            6 => {
                put16(&mut m, 12, 1);
                put16(&mut m, 14, 0);
            }
            7 => put16(&mut m, 12, 4),
            8 => put32(&mut m, 8, 65537),
            9 => put64(&mut m, 0, 0x3fff_ffff),
            _ => unreachable!(),
        }
        let before_ram = m.clone();
        let before_queue = serde_json::to_value(&q).unwrap();
        assert!(
            q.validated_chain(&m, 0x4000_0000, policy()).is_err(),
            "defect {defect}"
        );
        assert_eq!(m, before_ram);
        assert_eq!(serde_json::to_value(&q).unwrap(), before_queue);
    }
}

#[test]
fn scatter_receive_checks_entire_chain_before_writing() {
    let (q, mut m) = fixture(0);
    put16(&mut m, 12, 3);
    put16(&mut m, 14, 1);
    put64(&mut m, 16, DATA as u64 + 32);
    put32(&mut m, 24, 2);
    // Wrong direction in the last segment must not write the first one.
    let chain = q.validated_chain(&m, 0, policy()).unwrap().unwrap();
    let before = m.clone();
    assert!(chain.write(&mut m, b"abcde").is_err());
    assert_eq!(m, before);
    put16(&mut m, 28, 2);
    let chain = q.validated_chain(&m, 0, policy()).unwrap().unwrap();
    let before = m.clone();
    assert!(chain.write(&mut m, b"abcdef").is_err());
    assert_eq!(m, before);
    chain.write(&mut m, b"HELLO").unwrap();
    assert_eq!(&m[DATA..DATA + 3], b"HEL");
    assert_eq!(&m[DATA + 32..DATA + 34], b"LO");
}

#[test]
fn serialized_queue_keeps_the_retained_arm_field_names_and_order() {
    let (q, _) = fixture(0);
    assert_eq!(serde_json::to_string(&q).unwrap(), "{\"num\":8,\"ready\":true,\"desc\":0,\"avail\":4096,\"used\":4352,\"last_avail\":0,\"used_idx\":0}");
}
