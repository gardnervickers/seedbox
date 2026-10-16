//! Host-only closed bridge persistence. JSON constructs trusted test DTOs only;
//! no JSON parsing is used by the public snapshot reader.
use seedbox_machine::{
    pv::bridge::GuestBridge,
    snapshot::{CodecError, DecodeLimits, Reader, Writer},
};
use serde_json::{json, Value};
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 1 << 20,
        allocation_bytes: 1 << 20,
        collection_items: 20000,
        total_items: 40000,
        string_bytes: 20000,
        blob_bytes: 20000,
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
fn decode(bytes: &[u8], reached: u64) -> Result<GuestBridge, CodecError> {
    let mut input = bytes;
    let mut r = Reader::new(&mut input, bytes.len() as u64, 0, &limits())?;
    let b = GuestBridge::read_snapshot(&mut r, reached, false)?;
    r.finish()?;
    Ok(b)
}
fn active() -> Value {
    let mut v = fixture();
    v["codec"]["oracle"] = json!({"relay":4,"session":2,"public":1,"failed":false,"last_time":7,"pending":{"relay":4,"session":2,"request":1},"reply_prepared":false});
    v["codec"]["last_time"] = json!(9);
    v["session_highwater"] = json!(5);
    v["session"] = json!({"wire":2,"public":5});
    v
}
#[test]
fn exact_continuations_partial_commands_oracle_reset_and_superseded_preparations() {
    let mut commands = fixture();
    commands["codec"]["partial"] = json!(b"SBX_RPC1 1".to_vec());
    commands["codec"]["pending"] = json!([[9, "start"], [2, "step"]]);
    commands["codec"]["used_ids"] = json!([1, 2, 9]);
    let mut reset = fixture();
    reset["session_highwater"] = json!(99);
    let mut prepared = active();
    prepared["codec"]["oracle"]["reply_prepared"] = json!(true);
    prepared["prepared_replies"] =
        json!([{"relay":1,"session":1,"request":1},{"relay":4,"session":2,"request":1}]);
    for v in [fixture(), commands, reset, active(), prepared] {
        let bytes = encode(v);
        let original = bytes.clone();
        let b = decode(&bytes, 9).unwrap();
        let mut again = vec![];
        b.write_snapshot(&mut Writer::new(&mut again)).unwrap();
        assert_eq!(again, original);
    }
    let b = decode(&encode(active()), 9).unwrap();
    b.validate_snapshot(9, true).unwrap();
}
#[test]
fn reject_poison_actions_duplicate_pending_partial_time_and_correlation_relations() {
    let bads: Vec<(&str, Value)> = vec![
        ("codec poison", {
            let mut v = fixture();
            v["codec"]["failed"] = json!(true);
            v
        }),
        ("oracle poison", {
            let mut v = fixture();
            v["codec"]["oracle"]["failed"] = json!(true);
            v
        }),
        ("partial newline", {
            let mut v = fixture();
            v["codec"]["partial"] = json!([10]);
            v
        }),
        ("partial nul", {
            let mut v = fixture();
            v["codec"]["partial"] = json!([0]);
            v
        }),
        ("partial bound", {
            let mut v = fixture();
            v["codec"]["partial"] = json!(vec![65u8; 16449]);
            v
        }),
        ("unknown action", {
            let mut v = fixture();
            v["codec"]["pending"] = json!([[1, "explode"]]);
            v["codec"]["used_ids"] = json!([1]);
            v
        }),
        ("unreserved command", {
            let mut v = fixture();
            v["codec"]["pending"] = json!([[1, "start"]]);
            v
        }),
        ("duplicate pending", {
            let mut v = fixture();
            v["codec"]["pending"] = json!([[1, "start"], [1, "stop"]]);
            v["codec"]["used_ids"] = json!([1]);
            v
        }),
        ("reached", {
            let mut v = active();
            v["codec"]["last_time"] = json!(10);
            v
        }),
        ("oracle time", {
            let mut v = active();
            v["codec"]["last_time"] = json!(6);
            v
        }),
        ("oracle tuple", {
            let mut v = active();
            v["codec"]["oracle"]["public"] = json!(0);
            v
        }),
        ("pending correlation", {
            let mut v = active();
            v["codec"]["oracle"]["pending"]["request"] = json!(2);
            v
        }),
        ("session absent", {
            let mut v = active();
            v["session"] = Value::Null;
            v
        }),
        ("wire mapping", {
            let mut v = active();
            v["session"]["wire"] = json!(3);
            v
        }),
        ("public highwater", {
            let mut v = active();
            v["session"]["public"] = json!(4);
            v
        }),
        ("prepared missing", {
            let mut v = active();
            v["codec"]["oracle"]["reply_prepared"] = json!(true);
            v
        }),
        ("unprepared key", {
            let mut v = active();
            v["prepared_replies"] = json!([{"relay":4,"session":2,"request":1}]);
            v
        }),
        ("duplicate prepared", {
            let mut v = active();
            v["prepared_replies"] =
                json!([{"relay":1,"session":1,"request":1},{"relay":1,"session":1,"request":1}]);
            v
        }),
    ];
    for (label, v) in bads {
        assert!(decode(&encode(v), 9).is_err(), "{label}");
    }
    let mut v = active();
    v["codec"]["oracle"]["reply_prepared"] = json!(true);
    v["prepared_replies"] = json!([{"relay":4,"session":2,"request":1}]);
    let b = decode(&encode(v), 9).unwrap();
    assert!(b.validate_snapshot(9, true).is_err());
}
#[test]
fn strict_used_ids_unknown_tags_all_prefixes_and_trailing() {
    let mut v = fixture();
    v["codec"]["used_ids"] = json!([2, 9]);
    let bytes = encode(v);
    // partial count, pending count, then used count and its two u32 identities.
    for second in [0u32, 1, 2] {
        let mut bad = bytes.clone();
        bad[28..32].copy_from_slice(&second.to_le_bytes());
        assert!(decode(&bad, 0).is_err());
    }
    for prefix in 0..bytes.len() {
        assert!(decode(&bytes[..prefix], 0).is_err(), "prefix {prefix}");
    }
    for byte in [2u8, 255] {
        let mut bad = encode(fixture());
        bad[24] = byte;
        assert!(decode(&bad, 0).is_err());
    }
    let mut bad = bytes;
    bad.push(0);
    assert!(decode(&bad, 0).is_err());
}
#[test]
fn quota_checks_before_body_reads_and_explicit_io_errors() {
    struct HeaderOnly {
        head: std::io::Cursor<Vec<u8>>,
    }
    impl std::io::Read for HeaderOnly {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                (self.head.position() as usize) < self.head.get_ref().len(),
                "body must not be read"
            );
            std::io::Read::read(&mut self.head, b)
        }
    }
    let mut head = HeaderOnly {
        head: std::io::Cursor::new(u64::MAX.to_le_bytes().to_vec()),
    };
    let mut r = Reader::new(&mut head, 1 << 19, 0, &limits()).unwrap();
    assert!(GuestBridge::read_snapshot(&mut r, 0, false).is_err());
    let mut head = HeaderOnly {
        head: std::io::Cursor::new(100u64.to_le_bytes().to_vec()),
    };
    let mut l = limits();
    l.allocation_bytes = 99;
    let mut r = Reader::new(&mut head, 1000, 0, &l).unwrap();
    assert!(matches!(
        GuestBridge::read_snapshot(&mut r, 0, false),
        Err(CodecError::Limit(_))
    ));
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("read failure"))
        }
    }
    let mut broken = Broken;
    let mut r = Reader::new(&mut broken, 1000, 0, &limits()).unwrap();
    assert!(matches!(
        GuestBridge::read_snapshot(&mut r, 0, false),
        Err(CodecError::Io(_))
    ));
}
