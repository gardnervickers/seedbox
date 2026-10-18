//! Dropping the async owner must cancel and join an already running blocking
//! backend. The inert executor supplies no native witness; channels establish
//! paused/entered and return boundaries, never host sleeps or speed thresholds.
#![cfg(unix)]
mod support;

use seedbox_api::{ExecutionTermination, Phase};
use seedbox_application::{
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest,
        PreparedInputs, StopReason,
    },
    store::ContentStore,
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{
    num::NonZeroUsize,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

struct Provider {
    entered: Arc<Mutex<Option<oneshot::Sender<CancellationToken>>>>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
    dropped: Option<oneshot::Sender<()>>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(());
        }
    }
}
impl BackendProvider for Provider {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: seedbox_spec::backend::CpuBackend,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("resolved fixture must not prepare images")
    }
    fn retained(
        &self,
        inputs: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
        Ok(Box::new(Paused {
            profile: inputs.profile.clone(),
            entered: self.entered.clone(),
            release: self.release.clone(),
        }))
    }
}
struct Paused {
    profile: ExecutionProfile,
    entered: Arc<Mutex<Option<oneshot::Sender<CancellationToken>>>>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
}
impl ExecutionBackend for Paused {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn execute(
        &self,
        request: ExecutionRequest<'_>,
    ) -> seedbox_application::Result<ExecutionReport> {
        let entered = self.entered.lock().unwrap().take().unwrap();
        assert!(request.start_paused);
        entered.send(request.cancellation.clone()).unwrap();
        // An explicit permit holds this blocking execution at a known paused
        // boundary until the test has observed the owner's Drop cancellation.
        self.release.lock().unwrap().recv().unwrap();
        assert!(
            request.cancellation.is_cancelled(),
            "paused backend was not cancelled"
        );
        Ok(ExecutionReport {
            reason: StopReason::Cancelled,
            workload_exit: None,
            detail: None,
            witness: None,
            full_state: None,
            faults: vec![],
            host_wait_ns: 0,
            simulated_time_ns: Some(42),
        })
    }
}
struct FailureCleanup {
    manager: Arc<Manager>,
    release: mpsc::Sender<()>,
}
impl Drop for FailureCleanup {
    fn drop(&mut self) {
        // A regression must fail, not deadlock runtime teardown. This fallback
        // runs only after the assertion inspecting production cancellation.
        let _ = self.manager.cancel_active();
        let _ = self.release.send(());
    }
}

async fn owner_abort(serve: bool) {
    // Preserve uncertain inputs on any assertion failure; remove only after the
    // final service owner has gone and its blocking execution has returned.
    let root = tempfile::tempdir().unwrap().keep();
    let store = ContentStore::new(root.join("store"));
    store.put(b"catalogue-only-binary").unwrap();
    let manager = Arc::new(Manager::new(Catalogue::in_memory()));
    let (entered_send, entered) = oneshot::channel();
    let (release, receiver) = mpsc::channel();
    let (provider_dropped_send, provider_dropped) = oneshot::channel();
    let service = Arc::new(
        ExecutionService::new(
            manager.clone(),
            root.join("execution"),
            store,
            Arc::new(Provider {
                entered: Arc::new(Mutex::new(Some(entered_send))),
                release: Arc::new(Mutex::new(receiver)),
                dropped: Some(provider_dropped_send),
            }),
        )
        .unwrap(),
    );
    let weak_service = Arc::downgrade(&service);
    let _failure_cleanup = FailureCleanup {
        manager: manager.clone(),
        release: release.clone(),
    };
    let shutdown = CancellationToken::default();
    let mut request = support::resolved_request("abort-paused-owner");
    request.options.start_paused = true;
    let (owner, id, client, _sender) = if serve {
        let socket = root.join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let owner = tokio::spawn(seedbox_server::serve(
            listener,
            manager.clone(),
            service.clone(),
            NonZeroUsize::MIN,
            shutdown.clone(),
        ));
        let client = seedbox_client::Client::connect_to(&socket).unwrap();
        let id = client.submit(&request).await.unwrap().metadata.id;
        (owner, id, Some(client), None)
    } else {
        let id = manager.submit(request).unwrap().0.metadata.id;
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender.send(id.clone()).await.unwrap();
        let owner = tokio::spawn(service.clone().run(
            receiver,
            NonZeroUsize::MIN,
            shutdown.clone(),
        ));
        (owner, id, None, Some(sender))
    };
    let token = entered
        .await
        .expect("backend entered at its paused boundary");
    assert!(!token.is_cancelled());
    let mut watch = manager.watch(&id).unwrap();
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    // This assertion precedes the fixture fallback and backend release. The old
    // implementation fails here deterministically, without leaving a live job.
    assert!(
        token.is_cancelled(),
        "owner abort did not cancel its blocking execution"
    );
    assert!(shutdown.is_cancelled());
    release.send(()).unwrap();
    drop(client);
    drop(service);
    while watch.borrow().status.phase != Phase::Completed {
        watch.changed().await.unwrap();
    }
    assert_eq!(
        watch.borrow().status.termination,
        Some(ExecutionTermination::Stopped)
    );
    // The provider is owned only by ExecutionService. Its destructor proves the
    // started blocking closure has released its final service Arc after return.
    provider_dropped
        .await
        .expect("blocking execution joined and dropped service");
    assert!(weak_service.upgrade().is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn dropping_execution_run_cancels_and_joins_a_paused_blocking_backend() {
    // Infrastructure hang guard only; all correctness boundaries use channels.
    tokio::time::timeout(Duration::from_secs(60), owner_abort(false))
        .await
        .unwrap();
}
#[tokio::test]
async fn aborting_public_serve_cancels_and_joins_a_paused_blocking_backend() {
    tokio::time::timeout(Duration::from_secs(60), owner_abort(true))
        .await
        .unwrap();
}
