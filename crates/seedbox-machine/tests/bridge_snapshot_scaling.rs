//! Pure closed-record regressions: identity scratch is explicitly charged;
//! functional large-count cases make no CPU-time-isolation or benchmark claim.
use seedbox_machine::{
    pv::bridge::GuestBridge,
    snapshot::{CodecError, DecodeLimits, Reader, Writer},
};
use serde_json::{json, Value};
use std::{
    io::{self, Read},
    num::NonZeroU32,
};
const MANY: u32 = 50_000;
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 32 << 20,
        collection_items: MANY as u64 + 8,
        total_items: 3 * MANY as u64 + 32,
        string_bytes: 64,
        blob_bytes: 1 << 20,
    }
}
fn fixture() -> Value {
    serde_json::to_value(GuestBridge::default()).unwrap()
}
fn encode(v: Value) -> Vec<u8> {
    let b: GuestBridge = serde_json::from_value(v).unwrap();
    let mut out = vec![];
    b.write_snapshot(&mut Writer::new(&mut out)).unwrap();
    out
}
fn decode(wire: &[u8], boundary: bool, l: &DecodeLimits) -> Result<GuestBridge, CodecError> {
    let mut input = wire;
    let mut r = Reader::new(&mut input, wire.len() as u64, 0, l)?;
    let bridge = GuestBridge::read_snapshot(&mut r, 1, boundary)?;
    r.finish()?;
    Ok(bridge)
}
fn pending(duplicate: bool) -> Value {
    let mut v = fixture();
    let mut ids: Vec<_> = (1..=MANY).rev().collect();
    if duplicate {
        ids[MANY as usize - 1] = MANY;
    }
    v["codec"]["pending"] = json!(ids
        .into_iter()
        .map(|id| json!([id, if id % 2 == 0 { "step" } else { "start" }]))
        .collect::<Vec<_>>());
    v["codec"]["used_ids"] = json!((1..=MANY + 3).collect::<Vec<_>>());
    v
}
fn prepared(duplicate: bool) -> Value {
    let mut v = fixture();
    let mut ids: Vec<_> = (1..=MANY).rev().collect();
    if duplicate {
        ids[MANY as usize - 1] = MANY;
    }
    v["codec"]["oracle"] = json!({"relay":MANY+1,"session":MANY+1,"public":1,"failed":false,"last_time":1,"pending":null,"reply_prepared":false});
    v["codec"]["last_time"] = json!(1);
    v["session_highwater"] = json!(1);
    v["session"] = json!({"wire":MANY+1,"public":1});
    v["prepared_replies"] = json!(ids
        .into_iter()
        .map(|id| json!({"relay":id,"session":1,"request":1}))
        .collect::<Vec<_>>());
    v
}
#[test]
fn large_pending_identity_index_preserves_admission_order_and_acknowledged_ids() {
    let wire = encode(pending(false));
    let b = decode(&wire, true, &limits()).unwrap();
    let mut out = vec![];
    b.write_snapshot(&mut Writer::new(&mut out)).unwrap();
    assert_eq!(out, wire);
    assert!(matches!(
        decode(&encode(pending(true)), true, &limits()),
        Err(CodecError::Invalid("pending command identity or action"))
    ));
}
#[test]
fn large_general_superseded_prepared_keys_are_preserved_and_duplicates_rejected() {
    let wire = encode(prepared(false));
    let b = decode(&wire, false, &limits()).unwrap();
    let mut out = vec![];
    b.write_snapshot(&mut Writer::new(&mut out)).unwrap();
    assert_eq!(out, wire);
    assert!(matches!(
        decode(&encode(prepared(true)), false, &limits()),
        Err(CodecError::Invalid("duplicate prepared reply"))
    ));
    // The trusted validator remains fallible and nonmutating; it rejects boundary
    // ownership before allocating/sorting either validation identity index.
    assert!(matches!(
        b.validate_snapshot(1, true),
        Err(CodecError::Invalid("prepared reply delivery ownership"))
    ));
}
struct PrefixOnly(io::Cursor<Vec<u8>>);
impl Read for PrefixOnly {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        assert!(
            (self.0.position() as usize) < self.0.get_ref().len(),
            "collection body must not be read"
        );
        self.0.read(b)
    }
}
#[test]
fn pending_scratch_bytes_and_item_quota_fail_before_reserving_or_reading_pending_body() {
    let n = MANY as u64;
    let mut prefix = 0u64.to_le_bytes().to_vec();
    prefix.extend(n.to_le_bytes());
    for storage in [false, true] {
        let mut l = limits();
        if storage {
            l.allocation_bytes = n
                * (std::mem::size_of::<(NonZeroU32, String)>() + std::mem::size_of::<NonZeroU32>())
                    as u64
                - 1;
        } else {
            l.total_items = 2 * n - 1;
        }
        let mut reader = PrefixOnly(io::Cursor::new(prefix.clone()));
        let mut r = Reader::new(&mut reader, 16 + 12 * n + 100, 0, &l).unwrap();
        assert!(
            matches!(GuestBridge::read_snapshot(&mut r,1,true),Err(CodecError::Limit(s)) if s==if storage{"allocation bytes"}else{"aggregate items"})
        );
    }
}
#[test]
fn prepared_boundary_nonzero_count_is_rejected_before_body_even_when_huge() {
    let wire = encode(fixture());
    for n in [1u64, MANY as u64, u64::MAX] {
        let mut prefix = wire[..wire.len() - 8].to_vec();
        prefix.extend(n.to_le_bytes());
        let mut reader = PrefixOnly(io::Cursor::new(prefix.clone()));
        let mut r = Reader::new(&mut reader, prefix.len() as u64, 0, &limits()).unwrap();
        assert!(matches!(
            GuestBridge::read_snapshot(&mut r, 1, true),
            Err(CodecError::Invalid("prepared reply delivery ownership"))
        ));
    }
}
#[test]
fn general_prepared_validation_copy_is_charged_before_reserve_or_element_read() {
    let wire = encode(fixture());
    let n = MANY as u64;
    let mut prefix = wire[..wire.len() - 8].to_vec();
    prefix.extend(n.to_le_bytes());
    for storage in [false, true] {
        let mut l = limits();
        if storage {
            l.allocation_bytes = 2 * n * 3 * std::mem::size_of::<NonZeroU32>() as u64 - 1;
        } else {
            l.total_items = 2 * n - 1;
        }
        let mut reader = PrefixOnly(io::Cursor::new(prefix.clone()));
        let mut r = Reader::new(&mut reader, prefix.len() as u64 + 12 * n, 0, &l).unwrap();
        assert!(
            matches!(GuestBridge::read_snapshot(&mut r,1,false),Err(CodecError::Limit(s)) if s==if storage{"allocation bytes"}else{"aggregate items"})
        );
    }
}
#[test]
fn byte_buffers_do_not_consume_nonbyte_collection_item_quota() {
    let mut v = fixture();
    v["codec"]["partial"] = json!(vec![65u8; 4096]);
    let wire = encode(v);
    let mut l = limits();
    l.collection_items = 0;
    l.total_items = 0;
    decode(&wire, true, &l).unwrap();
    l.blob_bytes = 4095;
    assert!(matches!(
        decode(&wire, true, &l),
        Err(CodecError::Limit("blob bytes"))
    ));
}
