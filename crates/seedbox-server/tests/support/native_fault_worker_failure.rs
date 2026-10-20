//! A real Driver worker failure while a public fault is active, not a test panic.
use super::*;
use seedbox_driver_execution::process_lifetime::WorkerFailureInjector;

pub async fn pv_fault_worker_failure() {
    let processes = Arc::new(Mutex::new(None));
    let captured = processes.clone();
    native_case(move |owner| Box::pin(async move {
        let session = owner.session(CpuBackend::Pv);
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let executable = release_executable(&checkout);
        let monitor = executable.parent().unwrap().parent().unwrap().join("debug/seedbox-lifetime-fixture");
        assert!(monitor.is_file(), "explicitly build the real test-support monitor fixture first: {}", monitor.display());
        let injector = WorkerFailureInjector::default();
        *session.worker_failure.lock().unwrap() = Some((monitor, injector.clone()));
        let (id, fault) = active_throttle(session).await;
        // /proc and pidfds only observe liveness. Only the original spawn owner
        // behind this weak injector can signal the actual retained Driver Child.
        *captured.lock().unwrap() = Some(native_process_probe::OwnedProcesses::capture(session.session.path()));
        eprintln!("requesting original real Driver worker failure after guest START and applied public fault: id={id} fault={fault}");
        injector.fail_worker().unwrap();
        assert_eq!(injector.fail_worker().unwrap_err().kind(), std::io::ErrorKind::NotConnected);
        let mut watch = session.client.watch(&id).await.unwrap();
        let completed = loop {
            let state = watch.next().await.expect("native failure watch ended before completion").unwrap();
            check_bound_at(&session.client, &id, session.backend, session.bound, state.status.simulated_time_ns, "the failed native worker was joined").await;
            if state.status.phase == Phase::Completed { break state; }
        };
        let Some(ExecutionTermination::Error { message }) = &completed.status.termination else {
            panic!("worker failure must remain an execution error, not an oracle verdict or stop: {:?}", completed.status);
        };
        assert!(message.contains(seedbox_application::execution::VMM_FAILURE_MESSAGE), "wrong failure: {message}");
        assert_eq!(completed.status.oracle_exit_code, None);
        assert_eq!(completed.status.verdict(), TestVerdict::Incomplete);
        let records = directories(&session.execution_root.join("runs").join(id.as_str())).into_iter()
            .filter_map(|path| seedbox_application::orchestration::inspect(&path).ok()).collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert!(records[0].report.as_ref().and_then(|report| report.witness.as_ref()).is_none(), "abrupt worker death must not fabricate a native witness");
        assert!(records[0].error.as_ref().unwrap().contains(seedbox_application::execution::VMM_FAILURE_MESSAGE));
        assert_eq!(session.launch_request(&id).seed, SEED);
        // Production runtime/scratch removal is authorized only after the real
        // monitor receipt. Also obtain the independent W2 ownership barriers.
        let _barriers = session.assert_cleanup();
        assert_eq!(injector.fail_worker().unwrap_err().kind(), std::io::ErrorKind::NotConnected);
        eprintln!("ordinary native failure published: id={id} termination={:?}; no oracle/witness, receipt-authorized scratch removed and W2 held", completed.status.termination);
    })).await;
    // native_case has now positively joined serve/blocking work, reacquired all
    // ownership barriers and removed only verified inputs. Never infer exit from
    // a timeout or use these observed process identifiers to send a signal.
    processes
        .lock()
        .unwrap()
        .take()
        .expect("worker failure never reached live native ownership capture")
        .assert_exited();
    eprintln!("real public-fault worker-failure cleanup proved: original Driver, monitor/anchor/descendants exited after normal joined cleanup");
}
