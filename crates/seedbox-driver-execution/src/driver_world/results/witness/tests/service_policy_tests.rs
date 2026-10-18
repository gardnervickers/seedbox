//! Model-only getter API availability, not PV/TCG execution or native absence proof.
//! Actual backend policy mapping is checked by the retained definition fixture.
use super::*;
use seedbox_machine::machine::{ExitReason, StateEvidence};

struct ColdServiceModel {
    policy: seedbox_machine::definition::BackendPolicy,
    getter_calls: Arc<AtomicUsize>,
}
impl Machine for ColdServiceModel {
    type Context = ();
    type Error = String;
    type Snapshot = ();
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        _: Timestamp,
    ) -> std::result::Result<StopReason, String> {
        if self.policy.boot_end_evidence {
            c.outbox
                .emit(seedbox_machine::pv::Output {
                    at: Timestamp::from_nanos(c.clock.now()),
                    payload: seedbox_machine::pv::Response::BootEnded {
                        ended_boot: 0,
                        disposition: EvidenceDisposition::Exited { code: 0 },
                        native_booted: false,
                        disk_lost_sectors: 0,
                        surviving_disk: None,
                        retired_state_digest: None,
                    },
                })
                .map_err(|_| "invalid model output time".to_owned())?;
        }
        Ok(StopReason::Exited {
            reached: Timestamp::from_nanos(c.clock.now()),
            reason: ExitReason::ProgramExit { code: 0 },
        })
    }
    fn snapshot(&mut self) -> std::result::Result<(), String> {
        Ok(())
    }
    fn execution_evidence(&mut self) -> std::result::Result<Option<ExecutionEvidence>, String> {
        self.getter_calls.fetch_add(1, Ordering::SeqCst);
        Ok(None) // Cold mock never owned a native boot: do not fabricate a witness.
    }
    fn state_evidence(&mut self) -> std::result::Result<Option<StateEvidence>, String> {
        self.getter_calls.fetch_add(1, Ordering::SeqCst);
        Ok(None) // No native RAM, independently of report-output selection.
    }
}
#[test]
fn cold_mock_without_reports_keeps_evidence_getter_apis_available() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut oracle = factory_definition::definition(0);
    oracle.name = "oracle".into();
    let mut driver = Driver::new_seeded(
        Timestamp::from_nanos(0),
        std::time::Duration::from_nanos(10),
        1,
        7,
        || Ok(()),
        oracle,
        vec![],
        vec![],
        seedbox_driver::driver::NetworkConfig::default(),
        move |_| {
            Ok(ColdServiceModel {
                policy: seedbox_machine::definition::BackendPolicy {
                    boot_end_evidence: false,
                    ..Default::default()
                },
                getter_calls: calls.clone(),
            })
        },
    )
    .unwrap();
    driver.step(Timestamp::from_nanos(10)).unwrap();
    assert!(driver.drain_events().iter().all(|event| !matches!(event.kind,
        seedbox_driver::driver::EventKind::Output(ref output) if matches!(output.payload, seedbox_machine::pv::Response::BootEnded { .. })
    )), "cold mock emits no boot-end output");
    driver
        .with_barrier(|barrier| {
            assert!(barrier.execution_evidence(MachineId::ORACLE)?.is_none());
            assert!(barrier.state_evidence(MachineId::ORACLE)?.is_none());
            Ok(())
        })
        .unwrap();
    assert_eq!(observed.load(Ordering::SeqCst), 2);
}
#[test]
fn existing_interaction_component_still_collects_without_boot_end_reports() {
    let (mut driver, names, reads) = driver(Some(component()), false);
    driver.step(Timestamp::from_nanos(10)).unwrap();
    assert!(driver.drain_events().iter().all(|event| !matches!(event.kind,
        seedbox_driver::driver::EventKind::Output(ref output) if matches!(output.payload, seedbox_machine::pv::Response::BootEnded { .. })
    )));
    collect(
        &mut driver,
        &names,
        &ContentDigest::of(b"model driver"),
        None,
    )
    .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}
