//! A real Driver worker failure while a public fault is active, not a test panic.
use super::*;
use seedbox_application::process_lifetime::WorkerFailureInjector;

pub async fn pv_fault_worker_failure() {
    let processes = Arc::new(Mutex::new(None));
    let captured = processes.clone();
    native_case(move |owner| Box::pin(async move {
        let session = owner.session(DriverBackend::Pv);
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let executable = release_executable(&checkout);
        let monitor = executable.parent().unwrap().parent().unwrap().join("debug/seedbox-lifetime-fixture");
        assert!(monitor.is_file(), "explicitly build the real test-support monitor fixture first: {}", monitor.display());
        let injector = WorkerFailureInjector::default();
        *session.worker_failure.lock().unwrap() = Some((monitor, injector.clone()));
        let mut request = cpu_request(session);
        request.options.start_paused = true;
        let id = session.submit(request, 1).await;
        session.worker(&id).await;
        session.control(&id, "failure-initial-pause", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let output = until_output(session, &id, "START=").await;
        assert!(!output.contains("END="), "native target ended before the failure scenario");
        let fault = inject(session, &id, "failure-throttle", FaultAction::Throttle {
            handle: HANDLE.into(), machine: TARGET.into(), factor_permille: 2_000,
        }).await;
        step(session, &id).await;
        let world = session.client.world(&id).await.unwrap();
        assert_eq!(world.machines.iter().find(|machine| machine.name == TARGET).unwrap().throttle_permille, Some(2_000));
        applied_once(session, &id, &[fault]).await;
        let paused = session.client.playback(&id).await.unwrap().state;
        assert_eq!(paused.rate, PlaybackRate::Paused);
        assert!(!paused.terminal && !paused.stepping);
        assert!(!session.stdout(&id, TARGET, false).await.contains("END="));
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
        assert!(message.contains(seedbox_application::engine_adapter::VMM_FAILURE_MESSAGE), "wrong failure: {message}");
        assert_eq!(completed.status.oracle_exit_code, None);
        assert_eq!(completed.status.verdict(), TestVerdict::Incomplete);
        let records = directories(&session.execution_root.join("runs").join(id.as_str())).into_iter()
            .filter_map(|path| seedbox_application::orchestration::inspect(&path).ok()).collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert!(records[0].report.as_ref().and_then(|report| report.witness.as_ref()).is_none(), "abrupt worker death must not fabricate a native witness");
        assert!(records[0].error.as_ref().unwrap().contains(seedbox_application::engine_adapter::VMM_FAILURE_MESSAGE));
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
