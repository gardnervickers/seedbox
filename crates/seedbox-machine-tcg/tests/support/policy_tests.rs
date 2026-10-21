//! Public closed-codec and real Rust adapter tests. Child replies are explicitly
//! synthetic host QMP; these tests never start QEMU or execute guest code.
use crate::*;
fn configured(policy: BackendPolicy) -> Fixture {
    let mut f = Fixture::default();
    f.command.push_str(&format!(
        " seedbox.control={}",
        u8::from(policy.controller.as_deref() == Some("node"))
    ));
    f.policy = Some(("node".into(), policy));
    f
}
fn selected() -> BackendPolicy {
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
#[test]
fn exact_optional_named_policy_closes_without_selecting_defaults() {
    let p = selected();
    let (wire, _) = configured(p.clone()).wire();
    let s = decode(&wire);
    assert_eq!(encode(&s), wire);
    s.validate_policy("node", Some(&p)).unwrap();
    assert!(s.validate_policy("node", None).is_err());
    assert!(s.validate_policy("other", Some(&p)).is_err());
    let mut changes = Vec::new();
    for field in 0..9 {
        let mut q = p.clone();
        match field {
            0 => q.seed += 1,
            1 => q.timer_jitter_ns += 1,
            2 => q.stop_tolerance_ns += 1,
            3 => q.torn_writes = false,
            4 => q.full_state = false,
            5 => q.seeded_pv_speed = false,
            6 => q.boot_end_evidence = false,
            7 => q.controller = None,
            _ => q.controller = Some("other".into()),
        }
        changes.push(q);
    }
    for q in changes {
        assert!(s.validate_policy("node", Some(&q)).is_err());
    }
    let legacy = decode(&Fixture::default().wire().0);
    legacy.validate_policy("node", None).unwrap();
    assert!(legacy
        .validate_policy("node", Some(&BackendPolicy::default()))
        .is_err());
    let explicit = decode(&configured(BackendPolicy::default()).wire().0);
    assert!(explicit.validate_policy("node", None).is_err());
    explicit
        .validate_policy("node", Some(&BackendPolicy::default()))
        .unwrap();
}
#[test]
fn named_policy_requires_its_exact_controller_selector_before_restore() {
    let mut f = configured(selected());
    f.command = "quiet seedbox.control=0".into();
    reject(&f.wire().0, "controller selector");
    f.command = "quiet seedbox.control=1 seedbox.control=1".into();
    reject(&f.wire().0, "controller selector");
    f.command = "quiet".into();
    reject(&f.wire().0, "controller selector");
    f = configured(selected());
    f.logical_boot = 1;
    reject(&f.wire().0, "initial boot reservation");
}
#[test]
fn configured_never_booted_evidence_is_explicit_and_has_no_fabricated_ram() {
    let saved = decode(&configured(selected()).wire().0);
    let root = tempfile::tempdir().unwrap();
    let mut machine =
        TcgMachine::from_snapshot_in(Path::new("/never-launched"), &saved, root.path()).unwrap();
    assert_eq!(machine.state_evidence().unwrap(), None);
    assert_eq!(encode(&machine.snapshot().unwrap()), encode(&saved));
}
#[test]
fn migration_requires_a_physical_owner_and_no_unreserved_owner_mismatch() {
    let mut f = configured(selected());
    f.image = Some(b"synthetic migration".to_vec());
    f.pending = 0;
    let (wire, marks) = f.wire();
    let at = marks["active_boot"];
    let mut missing = wire.clone();
    missing[at] = 0;
    missing.drain(at + 1..at + 9);
    let len = missing.len() as u64 - 24;
    patch64(&mut missing, 16, len);
    reject(&missing, "active/logical boot ownership");
    let mut future = wire.clone();
    patch64(&mut future, at + 1, 1);
    reject(&future, "active/logical boot ownership");
    let mut stale = wire.clone();
    patch64(&mut stale, marks["logical_boot"], 1);
    reject(&stale, "active/logical boot ownership");
    let (cold, marks) = configured(selected()).wire();
    let at = marks["active_boot"];
    let mut phantom = cold;
    phantom[at] = 1;
    phantom.splice(at + 1..at + 1, [0; 8]);
    let len = phantom.len() as u64 - 24;
    patch64(&mut phantom, 16, len);
    reject(&phantom, "active/logical boot ownership");
}
#[cfg(feature = "test-support")]
mod process {
    use super::*;
    use seedbox_machine::{
        execution::ExecutionConfig,
        machine::{EvidenceDisposition, RunContext, Timestamp},
        pv::{Inbox, Input, Outbox, Output, Request, Response},
        rng::RngStream,
        test_support::FixedClock,
    };
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::{collections::VecDeque, fs, path::PathBuf};
    #[derive(Default)]
    struct Entropy(Vec<usize>);
    impl RngStream for Entropy {
        fn fill_bytes(&mut self, b: &mut [u8]) {
            self.0.push(b.len());
            b.fill(self.0.len() as u8);
        }
    }
    fn wrapper(root: &Path, mode: &str) -> (PathBuf, PathBuf) {
        let executable = root.join(format!("policy-fixture-{mode}"));
        let record = root.join(format!("{mode}.ndjson"));
        fs::write(
            executable.with_extension("config"),
            serde_json::to_vec(&serde_json::json!({"mode":mode,"record":record})).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_seedbox-tcg-scratch-fixture"),
            &executable,
        )
        .unwrap();
        (executable, record)
    }
    fn events(record: &Path) -> Vec<Value> {
        fs::read_to_string(record)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn commands(record: &Path) -> Vec<Value> {
        events(record)
            .into_iter()
            .filter(|v| v["kind"] == "command")
            .map(|v| v["command"].clone())
            .collect()
    }
    fn run(m: &mut TcgMachine, rng: &mut Entropy, requests: Vec<Request>) -> Vec<Output> {
        let at = Timestamp::from_nanos(0);
        let mut inbox: VecDeque<_> = requests
            .into_iter()
            .map(|payload| Input { at, payload })
            .collect();
        let mut out = Vec::new();
        let mut context = RunContext {
            worker: &mut (),
            clock: &FixedClock(0),
            rng,
            execution: ExecutionConfig::default(),
            inbox: Inbox::new(&mut inbox, at),
            outbox: Outbox::new(&mut out, at, at),
        };
        m.run(&mut context, at).unwrap();
        assert!(inbox.is_empty());
        out
    }
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn assert_reaped(record: &Path) {
        #[cfg(target_os = "linux")]
        for e in events(record).iter().filter(|v| v["kind"] == "start") {
            assert!(!Path::new("/proc")
                .join(e["pid"].as_u64().unwrap().to_string())
                .exists());
        }
    }
    #[test]
    fn host_fixture_serves_all_native_witness_commands_without_ram_capture() {
        let root = tempfile::tempdir().unwrap();
        let (exe, record) = wrapper(root.path(), "ok");
        let mut f = configured(selected());
        f.network = Some([2, 0, 0, 0, 0, 1]);
        let saved = decode(&f.wire().0);
        let mut machine = TcgMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        run(&mut machine, &mut Entropy::default(), vec![]);
        // Host-only, zero-budget fixture replies, not native witness evidence.
        let first = machine.execution_evidence().unwrap().unwrap();
        first.validate().unwrap();
        let synthetic_chain: [u8; 32] =
            Sha256::digest(b"synthetic host-only interaction chain").into();
        let pending = serde_json::to_vec(&(
            "tcg-pending-v1",
            "lifecycle-v1:0:0::0:",
            Some("net-v1:|"),
            Vec::<u8>::new(),
            Vec::<u8>::new(),
            GuestBridge::default(),
            false,
        ))
        .unwrap();
        assert_eq!(
            first.witness,
            hex::encode(
                seedbox_machine::machine::execution_evidence::chain::observe(
                    &synthetic_chain,
                    &pending,
                )
            )
        );
        assert_eq!(machine.execution_evidence().unwrap(), Some(first));
        drop(machine);
        assert_reaped(&record);
        let observed = commands(&record);
        for name in [
            "seedbox-witness-start",
            "seedbox-witness-chain",
            "seedbox-lifecycle-pending",
            "seedbox-net-pending",
        ] {
            assert!(observed.iter().any(|v| v["execute"] == name), "{name}");
        }
        assert!(!observed
            .iter()
            .any(|v| v["execute"] == "seedbox-x86-evidence"));
    }
    #[test]
    fn actual_adapter_caches_pre_loss_parts_pages_and_emits_each_retirement_once() {
        let root = tempfile::tempdir().unwrap();
        let (exe, record) = wrapper(root.path(), "ok");
        let p = selected();
        let s = decode(&configured(p.clone()).wire().0);
        let mut m = TcgMachine::from_snapshot_in(&exe, &s, root.path()).unwrap();
        let mut rng = Entropy::default();
        assert!(run(&mut m, &mut rng, vec![]).is_empty());
        assert_eq!(rng.0, vec![8]);
        let before = commands(&record).len();
        let live = m.state_evidence().unwrap().unwrap();
        let repeated = m.state_evidence().unwrap().unwrap();
        assert_eq!(live, repeated);
        assert_eq!(live.boot, 0);
        assert_eq!(live.disposition, EvidenceDisposition::Live);
        assert_eq!(live.page_hashes, Sha256::digest([0x42; 4096])[..8]);
        assert_eq!(commands(&record).len(), before + 2);
        let output = run(&mut m, &mut rng, vec![Request::Crash]);
        let retired = m.state_evidence().unwrap().unwrap();
        assert_eq!(retired.boot, 0);
        assert_eq!(retired.disposition, EvidenceDisposition::Crashed);
        assert_eq!(retired.page_hashes, live.page_hashes);
        assert!(
            matches!(&output[..],[Output{payload:Response::BootEnded{ended_boot:0,native_booted:true,disk_lost_sectors:0,surviving_disk:None,retired_state_digest:Some(d),..},..}] if d==&retired.digest)
        );
        let saved = m.snapshot().unwrap();
        let wire = encode(&saved);
        assert_eq!(encode(&decode(&wire)), wire);
        let records: Vec<_> = wire
            .windows(retired.page_hashes.len())
            .enumerate()
            .filter(|(_, v)| *v == retired.page_hashes.as_slice())
            .map(|(at, _)| at)
            .collect();
        assert_eq!(records.len(), 1);
        let mut corrupt = wire.clone();
        corrupt[records[0]] ^= 1;
        reject(&corrupt, "retired state evidence");
        drop(m);
        assert_reaped(&record);
        let restored = decode(&wire);
        restored.validate_policy("node", Some(&p)).unwrap();
        let mut m = TcgMachine::from_snapshot_in(&exe, &restored, root.path()).unwrap();
        assert_eq!(m.state_evidence().unwrap(), Some(retired));
        assert!(run(&mut m, &mut rng, vec![]).is_empty());
        assert!(run(&mut m, &mut rng, vec![Request::Reboot]).is_empty());
        assert_eq!(rng.0, vec![8, 8]);
        assert_eq!(m.state_evidence().unwrap().unwrap().boot, 1);
        let live1 = m.snapshot().unwrap();
        let bytes = encode(&live1);
        drop(m);
        assert_reaped(&record);
        let live1 = decode(&bytes);
        live1.validate_policy("node", Some(&p)).unwrap();
        let mut restored = TcgMachine::from_snapshot_in(&exe, &live1, root.path()).unwrap();
        assert!(run(&mut restored, &mut rng, vec![]).is_empty());
        assert_eq!(rng.0, vec![8, 8]);
        assert_eq!(restored.state_evidence().unwrap().unwrap().boot, 1);
        drop(restored);
        assert_reaped(&record);
        let c = commands(&record);
        let setups: Vec<_> = c
            .iter()
            .filter(|v| v["execute"] == "seedbox-timer-policy")
            .collect();
        assert_eq!(setups.len(), 3);
        assert_eq!(setups[0]["arguments"]["domain"], "timer-jitter/node");
        assert_eq!(setups[1]["arguments"]["domain"], "timer-jitter/node/1");
        assert_eq!(setups[1]["arguments"], setups[2]["arguments"]);
        let incoming = c
            .iter()
            .position(|v| v["execute"] == "migrate-incoming")
            .unwrap();
        assert_eq!(c[incoming - 1]["execute"], "query-seedbox-run");
        assert_eq!(c[incoming - 2]["execute"], "seedbox-timer-policy");
        assert_eq!(c[incoming - 3]["execute"], "qmp_capabilities");
    }
    #[test]
    fn nondefault_ordered_bridge_and_oracle_session_survive_pending_io_projection() {
        let mut value = serde_json::to_value(GuestBridge::default()).unwrap();
        value["codec"]["used_ids"] = serde_json::json!([9, 1, 7]);
        value["codec"]["pending"] = serde_json::json!([[1, "start"], [7, "step"]]);
        value["codec"]["last_time"] = serde_json::json!(0);
        value["codec"]["oracle"] = serde_json::json!({"relay":7,"session":8,"public":9,"failed":false,"last_time":0,"pending":{"relay":7,"session":8,"request":9},"reply_prepared":false});
        value["session_highwater"] = serde_json::json!(1);
        value["session"] = serde_json::json!({"wire":8,"public":1});
        let bridge: GuestBridge = serde_json::from_value(value).unwrap();
        let mut bridge_bytes = Vec::new();
        bridge
            .write_snapshot(&mut Writer::new(&mut bridge_bytes))
            .unwrap();
        let mut f = configured(selected());
        f.image = Some(b"synthetic live capture".to_vec());
        f.pending = 0;
        f.bridge_wire = Some(bridge_bytes);
        let root = tempfile::tempdir().unwrap();
        let (exe, record) = wrapper(root.path(), "ok");
        let saved = decode(&f.wire().0);
        let mut m = TcgMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        let first = m.state_evidence().unwrap().unwrap();
        let saved = decode(&encode(&m.snapshot().unwrap()));
        drop(m);
        let mut restored = TcgMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        assert_eq!(restored.state_evidence().unwrap(), Some(first.clone()));
        drop(restored);
        f.bridge_wire = None;
        let saved = decode(&f.wire().0);
        let mut clean = TcgMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        let plain = clean.state_evidence().unwrap().unwrap();
        assert_ne!(
            plain.parts.iter().find(|(n, _)| n == "pending_io"),
            first.parts.iter().find(|(n, _)| n == "pending_io")
        );
        drop(clean);
        assert_reaped(&record);
    }
    #[test]
    fn no_ram_crash_is_explicit_and_does_not_consume_boot_entropy_or_spawn() {
        let root = tempfile::tempdir().unwrap();
        let (exe, record) = wrapper(root.path(), "ok");
        let s = decode(&configured(selected()).wire().0);
        let mut m = TcgMachine::from_snapshot_in(&exe, &s, root.path()).unwrap();
        let mut rng = Entropy::default();
        let output = run(&mut m, &mut rng, vec![Request::Crash]);
        assert!(matches!(
            &output[..],
            [Output {
                payload: Response::BootEnded {
                    ended_boot: 0,
                    native_booted: false,
                    disposition: EvidenceDisposition::Crashed,
                    retired_state_digest: None,
                    ..
                },
                ..
            }]
        ));
        assert!(rng.0.is_empty());
        assert!(!record.exists());
        assert_eq!(m.state_evidence().unwrap(), None);
        assert!(run(&mut m, &mut rng, vec![Request::Crash]).is_empty());
        let saved = m.snapshot().unwrap();
        assert_eq!(encode(&decode(&encode(&saved))), encode(&saved));
    }
    #[test]
    fn malformed_metadata_and_page_files_are_errors_not_snapshot_digest_fallbacks() {
        for mode in ["bad-evidence-shape", "bad-page-file"] {
            let root = tempfile::tempdir().unwrap();
            let (exe, record) = wrapper(root.path(), mode);
            let saved = decode(&configured(selected()).wire().0);
            let mut m = TcgMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
            run(&mut m, &mut Entropy::default(), vec![]);
            let error = m.state_evidence().unwrap_err().to_string();
            assert!(
                error.contains("metadata") || error.contains("page"),
                "{error}"
            );
            drop(m);
            assert_reaped(&record);
        }
    }
    #[test]
    fn actual_atomic_loss_uses_sorted_latest_sectors_and_a_dedicated_boot_domain() {
        for torn in [false, true] {
            for seed in [0u64, 1, 2, 42] {
                let mut p = selected();
                p.seed = seed;
                p.torn_writes = torn;
                let mut original = Store::zeroed(4).unwrap();
                original.write(3, &[0x31; 512]).unwrap();
                original.flush();
                original.write(0, &[0x51; 512]).unwrap();
                original.write(0, &[0x72; 512]).unwrap();
                original.write(3, &[0x99; 512]).unwrap();
                let mut f = configured(p).disk(false);
                f.overlay = original.encode_overlays();
                let saved = decode(&f.wire().0);
                let root = tempfile::tempdir().unwrap();
                let mut m =
                    TcgMachine::from_snapshot_in(Path::new("/never-launched"), &saved, root.path())
                        .unwrap();
                let mut rng = Entropy::default();
                let out = run(&mut m, &mut rng, vec![Request::Crash]);
                assert!(rng.0.is_empty());
                let mut key = Sha256::new();
                key.update(b"seedbox-v1");
                key.update(seed.to_le_bytes());
                key.update(b"disk-loss/node/0");
                let key = key.finalize();
                let mut expected = original.clone();
                let mut count = 0u64;
                let (expected_store, lost) = original.crashed_keeping(|sector, bytes| {
                    assert_eq!(sector, if count == 0 { 0 } else { 3 });
                    assert_eq!(
                        bytes,
                        if count == 0 {
                            &[0x72; 512]
                        } else {
                            &[0x99; 512]
                        }
                    );
                    let mut h = Sha256::new();
                    h.update(key);
                    h.update(count.to_le_bytes());
                    count += 1;
                    let h = h.finalize();
                    torn && (u64::from_le_bytes(h[..8].try_into().unwrap()) as f64
                        / (u64::MAX as f64 + 1.0))
                        < 0.5
                });
                let bytes = encode(&m.snapshot().unwrap());
                let at = bytes.windows(8).position(|v| v == b"SBXSTOR1").unwrap();
                expected.replace_overlays(&bytes[at..]).unwrap();
                for sector in 0..4 {
                    let mut a = [0; 512];
                    let mut b = [0; 512];
                    expected.read(sector, &mut a).unwrap();
                    expected_store.read(sector, &mut b).unwrap();
                    assert_eq!(a, b, "seed={seed}, torn={torn}, sector={sector}");
                }
                let (_, again) =
                    expected.crashed_keeping(|_, _| panic!("loss left volatile sectors"));
                assert_eq!(again, 0);
                assert!(
                    matches!(&out[..],[Output{payload:Response::BootEnded{disk_lost_sectors:n,surviving_disk:Some(_),native_booted:false,..},..}]if *n==lost)
                );
            }
        }
    }
    #[test]
    fn bad_timer_policy_ack_fails_setup_before_run_and_reaps_child() {
        let root = tempfile::tempdir().unwrap();
        let (exe, record) = wrapper(root.path(), "bad-timer-ack");
        let saved = decode(&configured(selected()).wire().0);
        let mut m = TcgMachine::from_snapshot_in(&exe, &saved, root.path()).unwrap();
        let at = seedbox_machine::machine::Timestamp::from_nanos(0);
        let mut inbox = VecDeque::new();
        let mut out = Vec::new();
        let mut rng = Entropy::default();
        let error = m
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &FixedClock(0),
                    rng: &mut rng,
                    execution: ExecutionConfig::default(),
                    inbox: Inbox::new(&mut inbox, at),
                    outbox: Outbox::new(&mut out, at, at),
                },
                at,
            )
            .unwrap_err();
        assert!(error.to_string().contains("timer-policy reply"));
        assert!(m.snapshot().is_err());
        assert!(!commands(&record)
            .iter()
            .any(|v| v["execute"] == "migrate-incoming"
                || v["execute"] == "seedbox-advance"
                || v["execute"] == "seedbox-run"));
        drop(m);
        assert_reaped(&record);
    }
}
