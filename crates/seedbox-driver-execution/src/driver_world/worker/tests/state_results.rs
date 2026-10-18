//! Synthetic typed-backend fixtures: composition only, not native RAM evidence.
use super::super::super::results::publish_state;
use super::*;
use seedbox_machine::machine::{EvidenceDisposition, RamRegion, StateEvidence};

struct Ended(StateEvidence);
impl Machine for Ended {
    type Context = ();
    type Error = String;
    type Snapshot = ();
    fn run(
        &mut self,
        context: &mut seedbox_machine::machine::RunContext<'_, ()>,
        _: Timestamp,
    ) -> std::result::Result<MachineStop, String> {
        while context.inbox.take().is_some() {}
        Ok(MachineStop::Exited {
            reached: Timestamp::from_nanos(context.clock.now()),
            reason: match self.0.disposition {
                EvidenceDisposition::Exited { code } => ExitReason::ProgramExit { code },
                _ => panic!("synthetic ended fixture requires an exit disposition"),
            },
        })
    }
    fn snapshot(&mut self) -> std::result::Result<(), String> {
        Ok(())
    }
    fn state_evidence(&mut self) -> std::result::Result<Option<StateEvidence>, String> {
        Ok(Some(self.0.clone()))
    }
}
fn physical_worker(code: i32) -> (tempfile::TempDir, Worker<Ended>) {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let driver = Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        1,
        1,
        || Ok(()),
        definition("oracle"),
        vec![],
        vec![],
        NetworkConfig::default(),
        move |_| {
            let mut supplied = evidence();
            supplied.disposition = EvidenceDisposition::Exited { code };
            Ok(Ended(supplied))
        },
    )
    .unwrap();
    let state = wire::State {
        world: wire::World {
            seed: 1,
            machine_names: vec![],
            input_program: Default::default(),
            intervention_next: 0,
            readiness: wire::Readiness {
                initial: vec!["oracle".into()],
                seen: vec![],
                latched: false,
            },
            publication: Default::default(),
            position: Default::default(),
        },
    };
    let constructed = Arc::new(Mutex::new(VecDeque::from(["oracle".to_owned()])));
    let worker = Worker::new(
        driver,
        state,
        inventory(root.path()),
        output,
        BTreeMap::new(),
        constructed,
        None,
        |saved, writer| {
            serde_json::to_writer(writer, &saved)?;
            Ok(())
        },
    )
    .unwrap();
    (root, worker)
}
#[test]
fn unrepresentable_physical_exit_poisoning_prevents_retry_and_does_not_invent_a_verdict() {
    let (_root, mut worker) = physical_worker(-1);
    assert!(worker
        .step(Timestamp::from_nanos(10))
        .unwrap_err()
        .to_string()
        .contains("public workload-exit contract"));
    assert!(worker.failed && worker.terminal);
    assert!(worker.state.world.publication.oracle_exit.is_none());
    assert!(worker.step(Timestamp::from_nanos(20)).is_err());
}
#[test]
fn physical_oracle_program_exit_reaches_public_result_without_an_application_output() {
    let (_root, mut worker) = physical_worker(0);
    assert!(matches!(
        worker.step(Timestamp::from_nanos(10)).unwrap(),
        StepOutcome::Finished { .. }
    ));
    let report = worker
        .report(StopReason::Completed, None, None, None)
        .unwrap();
    assert_eq!(
        report.workload_exit,
        Some(WorkloadExit {
            service: "oracle".into(),
            code: 0
        })
    );
}
fn evidence() -> StateEvidence {
    let ram = vec![0xab; 4097];
    let mut regions = vec![];
    regions.extend_from_slice(&0x4000_0000u64.to_le_bytes());
    regions.extend_from_slice(&4097u64.to_le_bytes());
    let page_hashes = ram
        .chunks(4096)
        .flat_map(|page| {
            let mut digest = [0; 32];
            hex::decode_to_slice(ContentDigest::of(page).hex(), &mut digest).unwrap();
            digest[..8].to_vec()
        })
        .collect();
    StateEvidence {
        boot: 1,
        disposition: EvidenceDisposition::Exited { code: 0 },
        parts: vec![
            (
                "cpu".into(),
                ContentDigest::of(b"synthetic register fixture")
                    .hex()
                    .into(),
            ),
            ("ram".into(), ContentDigest::of(&ram).hex().into()),
            (
                "ram_regions".into(),
                ContentDigest::of(&regions).hex().into(),
            ),
        ],
        regions: vec![RamRegion {
            base: 0x4000_0000,
            length: 4097,
        }],
        page_hashes,
        digest: ContentDigest::of(b"synthetic backend-defined combined projection")
            .hex()
            .into(),
    }
}
#[test]
fn stopped_policy_component_is_retained_without_claiming_a_native_witness() {
    let (_root, mut worker, _) = worker(1);
    let digest =
        crate::driver_world::results::record_policy(&mut worker.driver, &worker.output).unwrap();
    let path = worker.output.join("driver-policy.json");
    let bytes = std::fs::read(&path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["kind"], "driver_policy");
    assert_eq!(value["sha256"], digest.hex());
    assert!(value["encoded_bytes"].as_u64().unwrap() > 0);
    assert!(
        crate::driver_world::results::record_policy(&mut worker.driver, &worker.output).is_err()
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert!(worker
        .report(StopReason::Completed, None, None, None)
        .unwrap()
        .witness
        .is_none());
}
#[test]
fn real_typed_getter_is_required_and_missing_evidence_never_publishes_a_report() {
    let (_root, mut worker, _) = worker(1);
    let error = publish_state(&mut worker.driver, &worker.names, &worker.output).unwrap_err();
    assert!(error.to_string().contains("not configured or supported"));
    assert!(!worker
        .output
        .join(seedbox_artifacts::evidence::STATE_REPORT)
        .exists());
}
#[test]
fn finished_driver_uses_retained_ended_evidence_and_paths_do_not_use_machine_names() {
    for workers in [1, 4] {
        let root = tempfile::tempdir().unwrap();
        let supplied = evidence();
        let expected_pages = supplied.page_hashes.clone();
        let mut driver = Driver::new_seeded(
            Timestamp::from_nanos(0),
            Duration::from_nanos(10),
            workers,
            1,
            || Ok(()),
            definition("../not-a-host-path"),
            vec![],
            vec![],
            NetworkConfig::default(),
            move |_| Ok(Ended(supplied.clone())),
        )
        .unwrap();
        let names: BTreeMap<_, _> = driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .map(|(id, _)| (id, "../not-a-host-path".to_owned()))
                    .collect())
            })
            .unwrap();
        assert!(matches!(
            driver.step(Timestamp::from_nanos(10)).unwrap(),
            StepOutcome::Finished { .. }
        ));
        let report = publish_state(&mut driver, &names, root.path()).unwrap();
        let capture = &report.machines["../not-a-host-path"];
        assert_eq!(capture.boot, 1);
        assert_eq!(
            capture.disposition,
            seedbox_artifacts::evidence::StateDisposition::Exited { code: 0 }
        );
        assert_eq!(capture.page_bytes().unwrap(), 16);
        assert_eq!(capture.page_log, "machines/state-0.pages");
        assert_eq!(
            std::fs::read(root.path().join(&capture.page_log)).unwrap(),
            expected_pages
        );
        let decoded: seedbox_artifacts::evidence::StateReport = serde_json::from_slice(
            &std::fs::read(root.path().join(seedbox_artifacts::evidence::STATE_REPORT)).unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, report);
        let before =
            std::fs::read(root.path().join(seedbox_artifacts::evidence::STATE_REPORT)).unwrap();
        assert!(
            publish_state(&mut driver, &names, root.path()).is_err(),
            "publication must not clobber prior evidence"
        );
        assert_eq!(
            std::fs::read(root.path().join(seedbox_artifacts::evidence::STATE_REPORT)).unwrap(),
            before
        );
    }
}
