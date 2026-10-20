//! Real client-only transitions and closed persistence; no worker or native VM.
#![cfg(target_arch = "x86_64")]
use super::*;
use crate::rng::RngStream;
use seedbox_machine::execution::ExecutionConfig;
use seedbox_machine::machine::{Machine, RunContext, Timestamp};
use seedbox_machine::pv::{Inbox, Input, Outbox, Output, Response};
use seedbox_machine::snapshot::DecodeLimits;
use seedbox_machine::test_support::FixedClock;
use std::{collections::VecDeque, path::Path};
fn machine() -> super::super::PVMachine {
    // PV places the initrd directly and therefore needs its protocol ceiling;
    // the common TCG header-only fixture intentionally does not set that field.
    let mut kernel = seedbox_machine::test_support::assets::bzimage_header();
    kernel[0x22c..0x230].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
    super::super::PVMachine::construct_with_memory(
        Path::new("/never-spawn"),
        kernel,
        b"initrd".to_vec(),
        "rdinit=/init".into(),
        None,
        None,
        8 << 20,
    )
    .unwrap()
}
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 8 << 20,
        allocation_bytes: 16 << 20,
        collection_items: 10000,
        total_items: 50000,
        string_bytes: 4096,
        blob_bytes: 4 << 20,
    }
}
struct NoEntropy;
impl RngStream for NoEntropy {
    fn fill_bytes(&mut self, _: &mut [u8]) {
        panic!("no native boot; no guest entropy draw is permitted")
    }
}
fn run(m: &mut super::super::PVMachine) -> Vec<Output> {
    let at = Timestamp::from_nanos(0);
    let mut queue: VecDeque<Input> = VecDeque::new();
    let mut outputs = Vec::new();
    m.run(
        &mut RunContext {
            worker: &mut (),
            clock: &FixedClock(0),
            rng: &mut NoEntropy,
            execution: ExecutionConfig::default(),
            inbox: Inbox::new(&mut queue, at),
            outbox: Outbox::new(&mut outputs, at, at),
        },
        at,
    )
    .unwrap();
    outputs
}
#[test]
fn no_ram_retirement_pending_report_and_named_policy_survive_closed_restore() {
    let mut m = machine();
    let p = BackendPolicy {
        seed: 42,
        full_state: true,
        boot_end_evidence: true,
        seeded_pv_speed: true,
        ..Default::default()
    };
    m.configure_policy("node", &p).unwrap();
    assert_eq!(m.state_evidence().unwrap(), None);
    m.request_crash().unwrap();
    assert_eq!(m.state_evidence().unwrap(), None);
    let snapshot = m.snapshot().unwrap();
    let mut bytes = Vec::new();
    snapshot.write_to(&mut bytes).unwrap();
    drop(m);
    let saved = super::super::Snapshot::read_from(&mut &bytes[..], &limits()).unwrap();
    saved.validate_policy("node", Some(&p)).unwrap();
    assert!(saved.validate_policy("other", Some(&p)).is_err());
    assert!(saved.validate_policy("node", None).is_err());
    let mut restored =
        super::super::PVMachine::from_snapshot(Path::new("/never-spawn"), &saved).unwrap();
    assert_eq!(restored.state_evidence().unwrap(), None);
    let out = run(&mut restored);
    assert!(matches!(
        &out[..],
        [Output {
            payload: Response::BootEnded {
                ended_boot: 0,
                native_booted: false,
                retired_state_digest: None,
                ..
            },
            ..
        }]
    ));
    assert!(run(&mut restored).is_empty());
}
#[test]
fn actual_client_loss_selects_latest_sectors_without_guest_entropy() {
    use seedbox_machine::definition::DiskImage;
    use seedbox_machine::machine::disk::Store;
    for torn in [false, true] {
        let mut m = machine();
        m.disk = Some(super::super::Disk::new(DiskImage::zeroed(2048).unwrap()));
        let mut store = Store::zeroed(4).unwrap();
        store.write(3, &[0x31; 512]).unwrap();
        store.flush();
        store.write(0, &[0x51; 512]).unwrap();
        store.write(0, &[0x72; 512]).unwrap();
        store.write(3, &[0x99; 512]).unwrap();
        m.disk.as_mut().unwrap().store = Some(store.clone());
        let p = BackendPolicy {
            seed: 42,
            torn_writes: torn,
            boot_end_evidence: true,
            ..Default::default()
        };
        m.configure_policy("node", &p).unwrap();
        m.request_crash().unwrap();
        let mut stream = Counter::named(42, b"disk-loss/", "node", Some(0));
        let (expected, lost) = store.crashed_keeping(|_, _| {
            torn && (stream.next().unwrap() as f64 / (u64::MAX as f64 + 1.0)) < 0.5
        });
        let actual = m.disk.as_ref().unwrap().store.as_ref().unwrap();
        for sector in 0..4 {
            let mut a = [0; 512];
            let mut b = [0; 512];
            actual.read(sector, &mut a).unwrap();
            expected.read(sector, &mut b).unwrap();
            assert_eq!(a, b);
        }
        assert_eq!(m.aux.pending.front().unwrap().lost, lost);
        let snapshot = m.snapshot().unwrap();
        let mut bytes = Vec::new();
        snapshot.write_to(&mut bytes).unwrap();
        let saved = super::super::Snapshot::read_from(&mut &bytes[..], &limits()).unwrap();
        saved.validate_policy("node", Some(&p)).unwrap();
    }
}
#[test]
fn real_private_configuration_preserves_absence_and_rejects_reconfiguration() {
    let mut m = machine();
    let legacy = m.snapshot().unwrap();
    legacy.validate_policy("node", None).unwrap();
    assert!(legacy
        .validate_policy("node", Some(&BackendPolicy::default()))
        .is_err());
    m.configure_policy("node", &BackendPolicy::default())
        .unwrap();
    let configured = m.snapshot().unwrap();
    configured
        .validate_policy("node", Some(&BackendPolicy::default()))
        .unwrap();
    assert!(configured.validate_policy("node", None).is_err());
    assert!(m
        .configure_policy("node", &BackendPolicy::default())
        .is_err());
}

#[test]
fn closed_normal_endpoint_survives_later_never_started_slot_without_fabricated_owner() {
    use seedbox_machine::machine::execution_evidence::chain;
    let mut m = machine();
    assert!(m.execution_evidence().unwrap().is_none());
    let witness = chain::observe(
        &chain::record(&chain::initial(), b'O', 1, 0, 0x3f8, 65),
        b"native pending",
    );
    m.aux.execution = Some(super::super::retained::EndedExecution {
        boot: 0,
        shutdown: false,
        witness,
        mark: 1,
        at: 0,
    });
    m.aux.logical = 2;
    m.aux.last = Some(super::super::retained::Ended {
        boot: 2,
        shutdown: false,
        native: false,
    });
    m.state = super::super::State::Crashed;
    m.pending_boots = 0;
    let expected = m.execution_evidence().unwrap().unwrap();
    assert_eq!(expected.boot, 0);
    let image = m.snapshot().unwrap();
    let mut wire = Vec::new();
    image.write_to(&mut wire).unwrap();
    let image = super::super::Snapshot::read_from(&mut &wire[..], &limits()).unwrap();
    let mut restored =
        super::super::PVMachine::from_snapshot(Path::new("/never-spawn"), &image).unwrap();
    assert_eq!(
        restored.execution_evidence().unwrap(),
        Some(expected.clone())
    );
    assert_eq!(restored.execution_evidence().unwrap(), Some(expected));
    assert_eq!(
        restored.witness_setup(3).unwrap().initial,
        chain::successor(&witness, 1, 0).unwrap()
    );
}
#[test]
fn successor_cache_carries_every_prior_native_incarnation_not_only_latest_suffix() {
    use seedbox_machine::machine::execution_evidence::chain;
    let mut endpoints = Vec::new();
    for first_byte in [65, 66] {
        let mut m = machine();
        let old = chain::observe(
            &chain::record(&chain::initial(), b'O', 1, 0, 0x3f8, first_byte),
            &[0],
        );
        m.aux.execution = Some(super::super::retained::EndedExecution {
            boot: 0,
            shutdown: false,
            witness: old,
            mark: 1,
            at: 4,
        });
        let new = m.witness_setup(1).unwrap().initial;
        let new = chain::observe(&chain::record(&new, b'H', 0, 8, 0, 0), &[0]);
        m.aux.execution = Some(super::super::retained::EndedExecution {
            boot: 1,
            shutdown: true,
            witness: new,
            mark: 3,
            at: 9,
        });
        endpoints.push(m.witness_setup(2).unwrap().initial);
    }
    assert_ne!(endpoints[0], endpoints[1]);
}
