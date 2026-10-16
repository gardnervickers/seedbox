use super::super::*;
use seedbox_machine::machine::{EvidenceDisposition, ExecutionEvidence};
use std::sync::atomic::{AtomicUsize, Ordering};
struct Native {
    reads: Arc<AtomicUsize>,
    fail_run: bool,
    fail_read: bool,
}
impl Machine for Native {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(&mut self, c: &mut RunContext<'_, ()>, _: Timestamp) -> Result<StopReason, Self::Error> {
        if self.fail_run {
            return Err(std::io::Error::other("uncertain native state"));
        }
        Ok(StopReason::Exited {
            reached: Timestamp::from_nanos(c.clock.now()),
            reason: ExitReason::Shutdown,
        })
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn execution_evidence(&mut self) -> Result<Option<ExecutionEvidence>, Self::Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_read {
            return Err(std::io::Error::other("native read failed"));
        }
        Ok(Some(ExecutionEvidence {
            boot: 2,
            disposition: EvidenceDisposition::Shutdown,
            witness: "a".repeat(64),
            disk: Some("b".repeat(64)),
        }))
    }
}
fn driver(fail_run: bool, fail_read: bool) -> (Driver<Native>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let m = Native {
        reads: reads.clone(),
        fail_run,
        fail_read,
    };
    (
        fixture::driver(
            Timestamp::from_nanos(0),
            Duration::from_nanos(100),
            1,
            SeededRng::new(0),
            || Ok(()),
            || Ok(vec![m]),
        )
        .unwrap(),
        reads,
    )
}
#[test]
fn finished_native_reads_are_stable_without_clock_events_or_ram() {
    let (mut d, reads) = driver(false, false);
    assert!(matches!(
        d.step(Timestamp::from_nanos(100)).unwrap(),
        StepOutcome::Finished { .. }
    ));
    let _ = d.drain_events();
    let at = d.now();
    let first = d
        .with_barrier(|b| b.execution_evidence(MachineId(0)))
        .unwrap()
        .unwrap();
    first.validate().unwrap();
    assert_eq!(
        first,
        d.with_barrier(|b| b.execution_evidence(MachineId(0)))
            .unwrap()
            .unwrap()
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(at, d.now());
    assert!(d.drain_events().is_empty());
}
#[test]
fn poison_rejected_before_native_read() {
    let (mut d, reads) = driver(true, false);
    assert!(d.step(Timestamp::from_nanos(100)).is_err());
    assert!(matches!(
        d.with_barrier(|b| b.execution_evidence(MachineId(0))),
        Err(Error::Failed)
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}
#[test]
fn stopped_allowed_unknown_identity_rejected() {
    let (mut d, reads) = driver(false, false);
    d.with_barrier(|b| {
        b.stop();
        b.execution_evidence(MachineId(0))
    })
    .unwrap()
    .unwrap()
    .validate()
    .unwrap();
    assert!(matches!(
        d.with_barrier(|b| b.execution_evidence(MachineId(99))),
        Err(Error::UnknownMachine(_))
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}
#[test]
fn native_failure_is_not_none_or_cached_success() {
    let (mut d, reads) = driver(false, true);
    assert!(matches!(
        d.with_barrier(|b| b.execution_evidence(MachineId(0))),
        Err(Error::Machine { .. })
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}
