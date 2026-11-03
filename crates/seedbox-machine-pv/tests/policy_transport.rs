//! Public codec/client interaction using an explicit synthetic host worker.
//! No KVM/HVF device, QEMU, native guest instruction or physical acceptance.
#![cfg(all(unix, target_arch = "x86_64", feature = "test-support"))]
use seedbox_machine::{
    definition::BackendPolicy,
    execution::ExecutionConfig,
    machine::{EvidenceDisposition, Machine, RunContext, Timestamp},
    pv::{bridge::GuestBridge, Inbox, Input, Outbox, Output, Request, Response},
    rng::RngStream,
    snapshot::{DecodeLimits, Writer},
    test_support::FixedClock,
};
use seedbox_machine_pv::machine::pv::{PVMachine, Snapshot};
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
};
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 32 << 20,
        collection_items: 1_000_000,
        total_items: 2_000_000,
        string_bytes: 4096,
        blob_bytes: 8 << 20,
    }
}
fn policy() -> BackendPolicy {
    BackendPolicy {
        seed: 42,
        timer_jitter_ns: 17,
        stop_tolerance_ns: 19,
        torn_writes: true,
        full_state: true,
        seeded_pv_speed: true,
        boot_end_evidence: true,
        controller: Some("node".into()),
    }
}
fn cold() -> Snapshot {
    cold_mode(true)
}
fn cold_mode(full: bool) -> Snapshot {
    let mut kernel = seedbox_machine::test_support::assets::bzimage_header();
    kernel[0x22c..0x230].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
    let initrd = b"opaque host fixture initrd";
    let mut body = Vec::new();
    let mut w = Writer::new(&mut body);
    w.u64(8 << 20).unwrap();
    w.u32(1).unwrap();
    w.string("rdinit=/init seedbox.control=1").unwrap();
    w.u64(0).unwrap();
    w.u64(0).unwrap();
    w.u8(0).unwrap();
    w.boolean(false).unwrap();
    w.boolean(false).unwrap();
    w.u64(1).unwrap();
    w.boolean(false).unwrap();
    w.u64(0).unwrap();
    w.bytes(&[]).unwrap();
    GuestBridge::default().write_snapshot(&mut w).unwrap();
    w.boolean(false).unwrap();
    w.boolean(false).unwrap();
    // Independent version3 policy/owner/full+execution-cache record, not private Rust inclusion.
    w.boolean(true).unwrap();
    w.string("node").unwrap();
    w.u64(42).unwrap();
    w.u64(17).unwrap();
    w.u64(19).unwrap();
    for flag in [true, full, true, true] {
        w.boolean(flag).unwrap();
    }
    w.boolean(true).unwrap();
    w.string("node").unwrap();
    w.u64(985).unwrap();
    w.u64(0).unwrap();
    w.boolean(false).unwrap(); // active
    w.boolean(false).unwrap(); // last
    w.boolean(false).unwrap(); // full cache
    w.boolean(false).unwrap(); // actual execution endpoint
    w.boolean(false).unwrap(); // retiring witness
    w.u64(0).unwrap();
    w.u8(0).unwrap();
    for n in [kernel.len() as u64, initrd.len() as u64, 0, 0, 0, 0] {
        w.u64(n).unwrap();
    }
    w.raw(&kernel).unwrap();
    w.raw(initrd).unwrap();
    let mut out = Vec::new();
    let mut w = Writer::new(&mut out);
    w.raw(b"SBXPVS01").unwrap();
    w.u32(1).unwrap();
    w.u8(1).unwrap();
    w.u8(0).unwrap();
    w.u16(0).unwrap();
    w.u64(body.len() as u64).unwrap();
    w.raw(&body).unwrap();
    Snapshot::read_from(&mut &out[..], &limits()).unwrap()
}
fn encode(s: &Snapshot) -> Vec<u8> {
    let mut b = Vec::new();
    s.write_to(&mut b).unwrap();
    b
}
fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    fixture_mode(root, "ok")
}
fn fixture_mode(root: &Path, mode: &str) -> (PathBuf, PathBuf) {
    let exe = root.join("pv-policy-fixture-ok");
    let record = root.join("record.ndjson");
    fs::write(
        exe.with_extension("config"),
        serde_json::to_vec(&serde_json::json!({"mode":mode,"record":record})).unwrap(),
    )
    .unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_seedbox-pv-scratch-fixture"), &exe).unwrap();
    (exe, record)
}
#[derive(Default)]
struct Entropy(Vec<usize>);
impl RngStream for Entropy {
    fn fill_bytes(&mut self, b: &mut [u8]) {
        self.0.push(b.len());
        b.fill(self.0.len() as u8);
    }
}
fn run(m: &mut PVMachine, rng: &mut Entropy, requests: Vec<Request>) -> Vec<Output> {
    let at = Timestamp::from_nanos(0);
    let mut q: VecDeque<_> = requests
        .into_iter()
        .map(|payload| Input { at, payload })
        .collect();
    let mut output = Vec::new();
    m.run(
        &mut RunContext {
            worker: &mut (),
            clock: &FixedClock(0),
            rng,
            execution: ExecutionConfig::default(),
            inbox: Inbox::new(&mut q, at),
            outbox: Outbox::new(&mut output, at, at),
        },
        at,
    )
    .unwrap();
    assert!(q.is_empty());
    output
}
fn records(p: &Path) -> Vec<Value> {
    fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn reaped(p: &Path) {
    #[cfg(target_os = "linux")]
    for e in records(p).iter().filter(|e| e["kind"] == "start") {
        assert!(!Path::new("/proc")
            .join(e["pid"].as_u64().unwrap().to_string())
            .exists());
    }
}
#[test]
fn real_client_query_end_cache_closed_restore_and_physical_boot_setup() {
    let saved = cold();
    saved.validate_policy("node", Some(&policy())).unwrap();
    assert!(saved.validate_policy("node", None).is_err());
    let root = tempfile::tempdir().unwrap();
    let (exe, record) = fixture(root.path());
    let mut m = PVMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
    assert_eq!(m.state_evidence().unwrap(), None);
    let mut rng = Entropy::default();
    assert!(run(&mut m, &mut rng, vec![]).is_empty());
    assert_eq!(rng.0, vec![8]);
    let live = m.state_evidence().unwrap().unwrap();
    assert_eq!(live.boot, 0);
    assert_eq!(live.disposition, EvidenceDisposition::Live);
    assert_eq!(live, m.state_evidence().unwrap().unwrap());
    let out = run(&mut m, &mut rng, vec![Request::Crash]);
    let ended = m.state_evidence().unwrap().unwrap();
    assert_eq!(ended.disposition, EvidenceDisposition::Crashed);
    assert_eq!(ended.page_hashes, live.page_hashes);
    assert!(
        matches!(&out[..],[Output{payload:Response::BootEnded{ended_boot:0,native_booted:true,retired_state_digest:Some(d),..},..}]if *d==ended.digest)
    );
    let bytes = encode(&m.snapshot().unwrap());
    drop(m);
    reaped(&record);
    let saved = Snapshot::read_from(&mut &bytes[..], &limits()).unwrap();
    assert_eq!(encode(&saved), bytes);
    saved.validate_policy("node", Some(&policy())).unwrap();
    let mut restored = PVMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
    assert_eq!(restored.state_evidence().unwrap(), Some(ended));
    assert!(run(&mut restored, &mut rng, vec![]).is_empty());
    assert!(run(&mut restored, &mut rng, vec![Request::Reboot]).is_empty());
    assert_eq!(rng.0, vec![8, 8]);
    assert_eq!(restored.state_evidence().unwrap().unwrap().boot, 1);
    // This mock image is deliberately not a native codec fixture. Direct borrowed
    // capture restore tests client boot authority; native cursor codec is unit-owned.
    let snapshot = restored.snapshot().unwrap();
    drop(restored);
    reaped(&record);
    let mut restored = PVMachine::from_snapshot_in(&exe, &snapshot, root.path()).unwrap();
    assert!(run(&mut restored, &mut rng, vec![]).is_empty());
    assert_eq!(rng.0, vec![8, 8]);
    drop(restored);
    reaped(&record);
    let events = records(&record);
    let boots: Vec<_> = events
        .iter()
        .filter_map(|e| e["command"].get("Boot"))
        .collect();
    assert_eq!(boots.len(), 3);
    assert_eq!(boots[0]["policy"]["boot"], 0);
    assert_eq!(boots[1]["policy"]["boot"], 1);
    assert_eq!(boots[1]["policy"], boots[2]["policy"]);
    assert_eq!(boots[2]["policy"]["speed_permille"], 985);
}

#[test]
fn normal_witness_is_identical_with_ram_diagnostics_off_and_survives_end_restore() {
    let mut expected = None;
    for full in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (exe, record) = fixture(root.path());
        let saved = cold_mode(full);
        let mut p = policy();
        p.full_state = full;
        saved.validate_policy("node", Some(&p)).unwrap();
        let mut m = PVMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        assert!(m.execution_evidence().unwrap().is_none());
        let mut rng = Entropy::default();
        assert!(run(&mut m, &mut rng, vec![]).is_empty());
        let live = m.execution_evidence().unwrap().unwrap();
        live.validate().unwrap();
        assert_eq!(Some(live.clone()), m.execution_evidence().unwrap());
        if let Some(ref e) = expected {
            assert_eq!(&live, e);
        } else {
            expected = Some(live);
        }
        let _ = run(&mut m, &mut rng, vec![Request::Crash]);
        let ended = m.execution_evidence().unwrap().unwrap();
        assert_eq!(ended.boot, 0);
        assert_eq!(ended.disposition, EvidenceDisposition::Crashed);
        let wire = encode(&m.snapshot().unwrap());
        drop(m);
        reaped(&record);
        let saved = Snapshot::read_from(&mut &wire[..], &limits()).unwrap();
        let mut m = PVMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        assert_eq!(Some(ended.clone()), m.execution_evidence().unwrap());
        assert_eq!(rng.0, vec![8]);
        drop(m);
        reaped(&record);
        if !full {
            assert!(records(&record)
                .iter()
                .all(|e| e["command"].get("Evidence").is_none()
                    && e["command"].get("Capture").is_none()));
        }
    }
}

/// Public-client stop/disposition model; actual PSCI instruction route is owned
/// by ARM worker tests and is not executable on this host toolchain.
#[test]
fn cached_ended_full_and_normal_disposition_match_same_boot_d6_for_every_cause() {
    for (mode, forced, expected) in [
        ("powered-off", false, EvidenceDisposition::Shutdown),
        ("psci-powered-off", false, EvidenceDisposition::Shutdown),
        ("reset", false, EvidenceDisposition::Crashed),
        ("triplefault", false, EvidenceDisposition::Crashed),
        ("ok", true, EvidenceDisposition::Crashed),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (exe, record) = fixture_mode(root.path(), mode);
        let mut m = PVMachine::from_snapshot_in(&exe, &cold(), root.path()).unwrap();
        let mut rng = Entropy::default();
        let mut out = run(&mut m, &mut rng, vec![]);
        if forced {
            out.extend(run(&mut m, &mut rng, vec![Request::Crash]));
        }
        let normal = m.execution_evidence().unwrap().unwrap();
        let full = m.state_evidence().unwrap().unwrap();
        assert_eq!(normal.boot, 0);
        assert_eq!(full.boot, normal.boot);
        assert_eq!(normal.disposition, expected);
        assert_eq!(full.disposition, normal.disposition);
        let reports: Vec<_> = out
            .iter()
            .filter_map(|o| match &o.payload {
                Response::BootEnded {
                    ended_boot,
                    disposition,
                    native_booted,
                    ..
                } => Some((*ended_boot, *disposition, *native_booted)),
                _ => None,
            })
            .collect();
        assert_eq!(reports, [(normal.boot, normal.disposition, true)], "{mode}");
        // A later forced crash cannot reclassify or duplicate a natural end.
        assert!(run(&mut m, &mut rng, vec![Request::Crash]).is_empty());
        assert_eq!(
            m.execution_evidence().unwrap().unwrap().disposition,
            expected
        );
        let saved = m.snapshot().unwrap();
        drop(m);
        reaped(&record);
        let bytes = encode(&saved);
        let saved = Snapshot::read_from(&mut &bytes[..], &limits()).unwrap();
        let mut m = PVMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        assert_eq!(m.execution_evidence().unwrap(), Some(normal));
        assert_eq!(m.state_evidence().unwrap(), Some(full));
        drop(m);
        reaped(&record);
    }
}
