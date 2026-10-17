//! Planned cancellation while the real guest is executing a public fault.
use super::*;

pub async fn pv_fault_cancel() {
    let processes = Arc::new(Mutex::new(None));
    let captured = processes.clone();
    native_case(move |owner| Box::pin(async move {
        let session = owner.session(DriverBackend::Pv);
        let (id, fault) = active_throttle(session).await;
        *captured.lock().unwrap() = Some(native_process_probe::OwnedProcesses::capture(session.session.path()));
        eprintln!("requesting ordinary public cancellation after guest START and applied fault: id={id} fault={fault}");
        session.client.stop(&id).await.unwrap();
        assert!(session.complete(&id, true, 1, SEED).await.is_none());
        let resource = session.client.describe(&id).await.unwrap();
        assert_eq!(resource.status.oracle_exit_code, None);
        assert_eq!(resource.status.verdict(), TestVerdict::Incomplete);
        assert_eq!(resource.status.termination, Some(ExecutionTermination::Stopped));
        eprintln!("ordinary fault-active cancellation joined: id={id}; no fabricated oracle result, W2 held and authorized scratch/runtime removal verified");
    })).await;
    // Serve and its blocking workers were positively joined by native_case.
    processes
        .lock()
        .unwrap()
        .take()
        .expect("cancellation never captured live native ownership")
        .assert_exited();
    eprintln!("public-fault cancellation cleanup proved: captured Driver/monitor/anchor/descendants exited and verified session removed");
}
