use super::*;
use seedbox_machine::snapshot::{DecodeLimits, Reader, Writer};
fn setup(boot: u64) -> BootPolicy {
    Named::new(
        "node",
        &BackendPolicy {
            seed: 42,
            timer_jitter_ns: 17,
            seeded_pv_speed: true,
            ..BackendPolicy::default()
        },
    )
    .unwrap()
    .setup(boot)
}
#[test]
fn boot_domains_and_legacy_speed_are_exact_and_speed_survives_reboots() {
    let mut a = Runtime::new(&setup(0)).unwrap();
    let mut b = Runtime::new(&setup(1)).unwrap();
    assert_eq!(a.setup.speed_permille, 985);
    assert_eq!(b.setup.speed_permille, 985);
    for (v, w) in [2, 16, 17, 9].into_iter().zip([1, 8, 15, 5]) {
        assert_eq!(a.arm(100).unwrap(), Some(100 + v));
        assert_eq!(b.arm(100).unwrap(), Some(100 + w));
    }
    assert_eq!(a.charge(1000).unwrap(), 985);
    assert_eq!(a.charge(1).unwrap(), 1);
}
#[test]
fn disable_and_zero_jitter_do_not_draw_or_change_deadlines() {
    let mut p = setup(0);
    p.timer_jitter_ns = 0;
    let mut r = Runtime::new(&p).unwrap();
    assert_eq!(r.arm(100).unwrap(), Some(100));
    assert_eq!(r.arm(0).unwrap(), None);
    assert_eq!(r.timer.counter, 0);
    let mut positive = Runtime::new(&setup(0)).unwrap();
    assert_eq!(positive.arm(0).unwrap(), None);
    assert_eq!(positive.timer.counter, 0);
    assert_eq!(positive.arm(50).unwrap(), Some(52));
    assert_eq!(positive.timer.counter, 1);
}
#[test]
fn closed_cursor_restore_continues_without_reconfiguration() {
    let mut r = Runtime::new(&setup(0)).unwrap();
    r.arm(100).unwrap();
    let mut bytes = Vec::new();
    r.encode(&mut Writer::new(&mut bytes)).unwrap();
    let limits = DecodeLimits {
        encoded_bytes: 8192,
        allocation_bytes: 16384,
        collection_items: 1024,
        total_items: 4096,
        string_bytes: 1024,
        blob_bytes: 4096,
    };
    let mut input = &bytes[..];
    let mut reader = Reader::new(&mut input, bytes.len() as u64, 0, &limits).unwrap();
    let mut restored = Runtime::decode(&mut reader).unwrap();
    reader.finish().unwrap();
    restored.validate().unwrap();
    assert_eq!(restored.timer.counter, 1);
    assert_eq!(r.arm(100).unwrap(), restored.arm(100).unwrap());
    assert_eq!(restored.timer.counter, 2);
    let mut changed = restored.clone();
    changed.setup.boot = 1;
    assert!(changed.validate().is_err());
}
#[test]
fn absent_default_and_every_named_authority_field_remain_distinct() {
    let p = BackendPolicy::default();
    let named = Named::new("node", &p).unwrap();
    authority(None, "node", None).unwrap();
    assert!(authority(None, "node", Some(&p)).is_err());
    assert!(authority(Some(&named), "node", None).is_err());
    assert!(authority(Some(&named), "other", Some(&p)).is_err());
    authority(Some(&named), "node", Some(&p)).unwrap();
    for q in [
        BackendPolicy {
            seed: 1,
            ..p.clone()
        },
        BackendPolicy {
            timer_jitter_ns: 1,
            ..p.clone()
        },
        BackendPolicy {
            stop_tolerance_ns: 1,
            ..p.clone()
        },
        BackendPolicy {
            torn_writes: true,
            ..p.clone()
        },
        BackendPolicy {
            full_state: true,
            ..p.clone()
        },
        BackendPolicy {
            seeded_pv_speed: true,
            ..p.clone()
        },
        BackendPolicy {
            boot_end_evidence: true,
            ..p.clone()
        },
        BackendPolicy {
            controller: Some("node".into()),
            ..p.clone()
        },
    ] {
        assert!(authority(Some(&named), "node", Some(&q)).is_err());
    }
}
#[test]
fn overflow_and_corrupt_saved_speed_are_errors_not_saturation() {
    let mut r = Runtime::new(&setup(0)).unwrap();
    r.timer.counter = u64::MAX;
    assert!(r.arm(1).is_err());
    let mut r = Runtime::new(&setup(0)).unwrap();
    assert!(r.arm(u64::MAX - 1).is_err());
    let mut named = Named::new("node", &BackendPolicy::default()).unwrap();
    named.speed_permille = 999;
    assert!(named.validate().is_err());
}
