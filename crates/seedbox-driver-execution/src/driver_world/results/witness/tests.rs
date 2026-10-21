//! Synthetic typed components test composition/framing only, NOT native proof.
use super::*;
use seedbox_machine::{
    machine::{RunContext, StopReason, Timestamp},
    test_support::factory_definition,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
fn component() -> ExecutionEvidence {
    use seedbox_machine::machine::execution_evidence::chain;
    let h = chain::record(&chain::initial(), b'O', 1, 17, 0x3f8, 65);
    ExecutionEvidence {
        boot: 2,
        disposition: EvidenceDisposition::Exited { code: 7 },
        witness: hex::encode(chain::observe(&h, b"pending")),
        disk: Some("d".repeat(64)),
    }
}
#[test]
fn insertion_order_optional_roles_and_current_disk_are_not_interchangeable() {
    let policy = ContentDigest::of(b"driver");
    let native = component();
    let state = ContentDigest::of(b"explicit fullstate component");
    let make = |reverse: bool, state: Option<&ContentDigest>, disk: Option<String>| {
        let mut frame = Frame::new(&policy);
        let mut e = native.clone();
        e.disk = disk;
        for name in if reverse { ["a", "z"] } else { ["z", "a"] } {
            frame.machine(name, &e, state).unwrap();
        }
        frame.finish().unwrap()
    };
    let normal = make(false, None, native.disk.clone());
    // Independently computed with Python hashlib + explicit LE struct packing.
    assert_eq!(
        normal.hex(),
        "b4288ad881f4b430db60b31cc3a143040fa561eac6193c08da7b8fe88858538a"
    );
    assert_ne!(normal, make(true, None, native.disk.clone()));
    assert_ne!(normal, make(false, Some(&state), native.disk.clone()));
    assert_ne!(normal, make(false, None, None));
    assert_ne!(normal, make(false, None, Some("e".repeat(64))));
    assert_eq!(normal, make(false, None, native.disk.clone()));
    let mut left = Frame::new(&policy);
    left.machine("z", &native, Some(&state)).unwrap();
    let mut swapped = native.clone();
    swapped.disk = Some(state.hex().into());
    let mut right = Frame::new(&policy);
    right
        .machine(
            "z",
            &swapped,
            Some(&ContentDigest::try_from(native.disk.clone().unwrap()).unwrap()),
        )
        .unwrap();
    assert_ne!(left.finish().unwrap(), right.finish().unwrap());
}
#[test]
fn policy_alone_duplicate_names_and_malformed_native_hashes_never_complete_a_witness() {
    let policy = ContentDigest::of(b"policy");
    assert!(Frame::new(&policy).finish().is_err());
    let native = component();
    let mut frame = Frame::new(&policy);
    assert!(frame.machine("", &native, None).is_err());
    frame.machine("one", &native, None).unwrap();
    assert!(frame.machine("one", &native, None).is_err());
    let mut invalid = native.clone();
    invalid.witness = "A".repeat(64);
    assert!(Frame::new(&policy).machine("one", &invalid, None).is_err());
    invalid = native;
    invalid.disk = Some("invalid".into());
    assert!(Frame::new(&policy).machine("one", &invalid, None).is_err());
}
struct Model {
    native: Option<ExecutionEvidence>,
    fail: bool,
    ram_reads: Arc<AtomicUsize>,
}
impl Machine for Model {
    type Context = ();
    type Error = String;
    type Snapshot = ();
    fn run(
        &mut self,
        _: &mut RunContext<'_, ()>,
        _upto: Timestamp,
    ) -> std::result::Result<StopReason, String> {
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> std::result::Result<(), String> {
        Err("composition test must not snapshot".into())
    }
    fn execution_evidence(&mut self) -> std::result::Result<Option<ExecutionEvidence>, String> {
        if self.fail {
            Err("actual getter failure".into())
        } else {
            Ok(self.native.clone())
        }
    }
    fn state_evidence(
        &mut self,
    ) -> std::result::Result<Option<seedbox_machine::machine::StateEvidence>, String> {
        self.ram_reads.fetch_add(1, Ordering::SeqCst);
        Err("normal capture must not ask for RAM".into())
    }
}
fn driver(
    native: Option<ExecutionEvidence>,
    fail: bool,
) -> (Driver<Model>, BTreeMap<MachineId, String>, Arc<AtomicUsize>) {
    driver_components(vec![(native.clone(), fail), (native, fail)])
}
fn driver_components(
    components: Vec<(Option<ExecutionEvidence>, bool)>,
) -> (Driver<Model>, BTreeMap<MachineId, String>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let shared = reads.clone();
    let mut oracle = factory_definition::definition(0);
    oracle.name = "z".into();
    let mut worker = factory_definition::definition(1);
    worker.name = "a".into();
    let mut components = components.into_iter();
    let mut driver = Driver::new_seeded(
        Timestamp::from_nanos(0),
        std::time::Duration::from_nanos(10),
        1,
        7,
        || Ok(()),
        oracle,
        vec![worker],
        vec![],
        seedbox_driver::driver::NetworkConfig::default(),
        move |_| {
            let (native, fail) = components.next().unwrap();
            Ok(Model {
                native,
                fail,
                ram_reads: shared.clone(),
            })
        },
    )
    .unwrap();
    let names = driver
        .with_barrier(|b| {
            Ok(b.machines()
                .enumerate()
                .map(|(ordinal, (id, _))| (id, if ordinal == 0 { "z".into() } else { "a".into() }))
                .collect())
        })
        .unwrap();
    (driver, names, reads)
}
#[test]
fn stopped_collection_keeps_insertion_order_and_never_forces_ram() {
    let native = component();
    let (mut driver, names, reads) = driver(Some(native.clone()), false);
    let policy = ContentDigest::of(b"driver policy");
    let actual = collect(&mut driver, &names, &policy, None).unwrap();
    let mut expected = Frame::new(&policy);
    expected.machine("z", &native, None).unwrap();
    expected.machine("a", &native, None).unwrap();
    assert_eq!(actual, expected.finish().unwrap());
    assert_eq!(actual, collect(&mut driver, &names, &policy, None).unwrap());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}
#[test]
fn optional_state_joins_the_actual_owner_but_never_supplies_current_surviving_disk() {
    use seedbox_artifacts::evidence::{RamRegion, StateCapture};
    let native = component();
    let policy = ContentDigest::of(b"policy");
    let (mut driver, names, reads) = driver(Some(native.clone()), false);
    let mut region = Vec::new();
    region.extend_from_slice(&0u64.to_le_bytes());
    region.extend_from_slice(&4096u64.to_le_bytes());
    let state_digest = ContentDigest::of(b"synthetic diagnostic component");
    let mut state = StateReport {
        version: 1,
        machines: ["z", "a"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                (
                    name.into(),
                    StateCapture {
                        boot: 2,
                        disposition: disposition(native.disposition),
                        digest: state_digest.clone(),
                        parts: BTreeMap::from([
                            ("ram".into(), ContentDigest::of(b"model-only RAM")),
                            ("ram_regions".into(), ContentDigest::of(&region)),
                            (
                                "disk".into(),
                                ContentDigest::try_from("e".repeat(64)).unwrap(),
                            ),
                        ]),
                        regions: vec![RamRegion {
                            base: 0,
                            length: 4096,
                        }],
                        page_log: format!("machines/{index}.pages"),
                    },
                )
            })
            .collect(),
    };
    let actual = collect(&mut driver, &names, &policy, Some(&state)).unwrap();
    let mut expected = Frame::new(&policy);
    expected.machine("z", &native, Some(&state_digest)).unwrap();
    expected.machine("a", &native, Some(&state_digest)).unwrap();
    assert_eq!(actual, expected.finish().unwrap());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    state.machines.get_mut("z").unwrap().boot = 3;
    assert!(collect(&mut driver, &names, &policy, Some(&state))
        .unwrap_err()
        .to_string()
        .contains("physical owner"));
    state.machines.get_mut("z").unwrap().boot = 2;
    state.machines.get_mut("z").unwrap().disposition =
        seedbox_artifacts::evidence::StateDisposition::Live;
    assert!(collect(&mut driver, &names, &policy, Some(&state)).is_err());
}
#[test]
fn none_backend_errors_and_bad_identity_fail_without_ram_or_policy_only_success() {
    let policy = ContentDigest::of(b"driver policy");
    for (native, fail, part) in [
        (None, false, "no physical boot"),
        (Some(component()), true, "actual getter failure"),
    ] {
        let (mut driver, names, reads) = driver(native, fail);
        let error = collect(&mut driver, &names, &policy, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains(part) && error.contains("z"), "{error}");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }
    let (mut driver, mut names, _) = driver(Some(component()), false);
    names.pop_first();
    assert!(collect(&mut driver, &names, &policy, None).is_err());
}

#[test]
fn ordered_validation_precedes_later_backend_errors_after_bulk_collection() {
    let policy = ContentDigest::of(b"driver policy");
    let mut malformed = component();
    malformed.witness = "invalid".into();
    for first in [None, Some(malformed)] {
        let expected_none = first.is_none();
        let (mut driver, names, reads) =
            driver_components(vec![(first, false), (Some(component()), true)]);
        let error = collect(&mut driver, &names, &policy, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("for z:"), "{error}");
        assert!(!error.contains("actual getter failure"), "{error}");
        assert_eq!(error.contains("no physical boot"), expected_none, "{error}");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }
}

mod network_checkpoint_tests;
mod service_policy_tests;
