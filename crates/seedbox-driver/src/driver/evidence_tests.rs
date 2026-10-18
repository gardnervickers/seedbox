//! Private stopped-barrier delegation and failure-policy tests.
use super::*;
use seedbox_machine::machine::{EvidenceDisposition, RamRegion, StateEvidence};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};

struct EvidenceMachine {
    reads: Arc<AtomicUsize>,
    ram: Vec<u8>,
    fail: bool,
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|v| format!("{v:02x}")).collect()
}
impl Machine for EvidenceMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Vec<u8>;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        _: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        if self.fail {
            return Err(std::io::Error::other("execution state uncertain"));
        }
        Ok(StopReason::Exited {
            reached: Timestamp::from_nanos(context.clock.now()),
            reason: ExitReason::Shutdown,
        })
    }
    fn snapshot(&mut self) -> Result<Vec<u8>, Self::Error> {
        Ok(self.ram.clone())
    }
    fn state_evidence(&mut self) -> Result<Option<StateEvidence>, Self::Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let region = [0u64.to_le_bytes(), (self.ram.len() as u64).to_le_bytes()].concat();
        let parts = vec![
            ("ram".into(), hex(Sha256::digest(&self.ram))),
            ("ram_regions".into(), hex(Sha256::digest(region))),
        ];
        let digest = hex(Sha256::digest(
            parts
                .iter()
                .flat_map(|(_, h)| h.bytes())
                .collect::<Vec<_>>(),
        ));
        Ok(Some(StateEvidence {
            boot: 1,
            disposition: EvidenceDisposition::Shutdown,
            parts,
            regions: vec![RamRegion {
                base: 0,
                length: self.ram.len() as u64,
            }],
            page_hashes: self
                .ram
                .chunks(4096)
                .flat_map(|page| Sha256::digest(page)[..8].to_vec())
                .collect(),
            digest,
        }))
    }
}
fn driver(fail: bool) -> (Driver<EvidenceMachine>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let machine = EvidenceMachine {
        reads: reads.clone(),
        ram: vec![7; 4096 + 17],
        fail,
    };
    (
        fixture::driver(
            Timestamp::from_nanos(0),
            Duration::from_nanos(100),
            1,
            SeededRng::new(0),
            || Ok(()),
            || Ok(vec![machine]),
        )
        .unwrap(),
        reads,
    )
}
#[test]
fn evidence_works_after_finished_without_execution_or_publication() {
    let (mut d, reads) = driver(false);
    assert!(matches!(
        d.step(Timestamp::from_nanos(100)).unwrap(),
        StepOutcome::Finished { .. }
    ));
    let before = d.drain_events();
    let at = d.now();
    let first = d
        .with_barrier(|b| b.state_evidence(MachineId(0)))
        .unwrap()
        .unwrap();
    first.validate().unwrap();
    assert_eq!(first.parts[0].1, hex(Sha256::digest(vec![7; 4096 + 17])));
    assert_eq!(first.page_hashes.len(), 16);
    assert_eq!(
        d.with_barrier(|b| b.state_evidence(MachineId(0)))
            .unwrap()
            .unwrap(),
        first
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(d.now(), at);
    assert!(d.drain_events().is_empty());
    assert!(!before.is_empty());
}
#[test]
fn poisoned_driver_never_calls_backend_evidence() {
    let (mut d, reads) = driver(true);
    assert!(d.step(Timestamp::from_nanos(100)).is_err());
    assert!(matches!(
        d.with_barrier(|b| b.state_evidence(MachineId(0))),
        Err(Error::Failed)
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}
#[test]
fn stopped_driver_evidence_and_unknown_identity() {
    let (mut d, reads) = driver(false);
    d.with_barrier(|b| {
        b.stop();
        b.state_evidence(MachineId(0))
    })
    .unwrap()
    .unwrap()
    .validate()
    .unwrap();
    assert!(matches!(
        d.with_barrier(|b| b.state_evidence(MachineId(99))),
        Err(Error::UnknownMachine(_))
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}
