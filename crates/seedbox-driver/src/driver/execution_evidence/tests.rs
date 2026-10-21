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
    assert!(matches!(
        d.with_barrier(|b| b.execution_evidence_all()),
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
    let mut all = d.with_barrier(|b| b.execution_evidence_all()).unwrap();
    assert_eq!(all.len(), 1);
    all.pop()
        .unwrap()
        .result
        .unwrap()
        .unwrap()
        .validate()
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 2);
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

// Synthetic callbacks prove dispatch/collection contracts; native correctness is
// checked separately through fresh workload witnesses and replays.
struct BulkNative {
    id: usize,
    gate: Option<Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>>,
    reads: Arc<AtomicUsize>,
    fail: u8,
}
impl Machine for BulkNative {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(&mut self, _: &mut RunContext<'_, ()>, _: Timestamp) -> Result<StopReason, Self::Error> {
        panic!("evidence collection must not execute machines");
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        panic!("execution evidence must not capture RAM");
    }
    fn execution_evidence(&mut self) -> Result<Option<ExecutionEvidence>, Self::Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            let (lock, signal) = &**gate;
            let mut entered = lock.lock().unwrap();
            *entered += 1;
            signal.notify_all();
            let (entered, _) = signal
                .wait_timeout_while(entered, Duration::from_secs(30), |n| *n < 3)
                .unwrap();
            assert_eq!(
                *entered, 3,
                "all evidence callbacks must enter before completing"
            );
        }
        match self.fail {
            1 => return Err(std::io::Error::other("evidence fixture failure")),
            2 => panic!("evidence fixture panic"),
            _ => {}
        }
        Ok(Some(ExecutionEvidence {
            boot: self.id as u64,
            disposition: EvidenceDisposition::Live,
            witness: format!("{:064x}", self.id + 1),
            disk: None,
        }))
    }
}
fn bulk_driver(workers: usize, mixed_failures: bool) -> (Driver<BulkNative>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let shared = reads.clone();
    let gate =
        (workers == 3).then(|| Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new())));
    let machines = (0..3)
        .map(|id| BulkNative {
            id,
            gate: gate.clone(),
            reads: shared.clone(),
            fail: if mixed_failures { [1, 2, 0][id] } else { 0 },
        })
        .collect();
    let driver = fixture::driver(
        Timestamp::from_nanos(0),
        Duration::from_nanos(100),
        workers,
        SeededRng::new(0),
        || Ok(()),
        || Ok(machines),
    )
    .unwrap();
    (driver, reads)
}
#[test]
fn bulk_reads_overlap_and_preserve_order_without_execution_or_publication() {
    for workers in [1, 3] {
        let (mut driver, reads) = bulk_driver(workers, false);
        let _ = driver.drain_events();
        let before = driver.now();
        let results = driver.with_barrier(|b| b.execution_evidence_all()).unwrap();
        assert_eq!(results.len(), 3);
        for (id, result) in results.into_iter().enumerate() {
            assert_eq!(result.machine, MachineId(id));
            let evidence = result.result.unwrap().unwrap();
            assert_eq!(evidence.boot, id as u64);
            assert_eq!(evidence.witness, format!("{:064x}", id + 1));
        }
        assert_eq!(reads.load(Ordering::SeqCst), 3);
        assert_eq!(driver.now(), before);
        assert!(driver.drain_events().is_empty());
        driver
            .with_barrier(|b| {
                for id in 0..3 {
                    assert_eq!(b.machine(MachineId(id))?.id, id);
                }
                Ok(())
            })
            .unwrap();
    }
}
#[test]
fn bulk_reads_join_and_restore_every_machine_after_failure_and_panic() {
    let (mut driver, reads) = bulk_driver(3, true);
    let results = driver.with_barrier(|b| b.execution_evidence_all()).unwrap();
    let mut results = results.into_iter();
    assert!(matches!(
        results.next().unwrap().result,
        Err(Error::Machine {
            id: MachineId(0),
            ..
        })
    ));
    assert!(matches!(
        results.next().unwrap().result,
        Err(Error::Panicked { id: MachineId(1) })
    ));
    assert_eq!(results.next().unwrap().result.unwrap().unwrap().boot, 2);
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    driver
        .with_barrier(|b| {
            for id in 0..3 {
                assert_eq!(b.machine(MachineId(id))?.id, id);
            }
            Ok(())
        })
        .unwrap();
}
