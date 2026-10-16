//! Host-only public-service composition tests. The injected executor is NOT a
//! native worker and produces no witness. These tests establish HTTP dispatch,
//! retained error/verdict separation and lifecycle ownership, not determinism,
//! native backend qualification, or checkpoint restoration.
#![cfg(unix)]

mod support;

use futures_util::StreamExt;
use seedbox_api::{ComparisonStatus, ErrorCode, ExecutionTermination, Phase, TestVerdict};
use seedbox_application::{
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest,
        PreparedInputs, StopReason, WorkloadExit,
    },
    store::{ContentDigest, ContentStore},
};
use seedbox_client::{Client, Error};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Behavior {
    MissingWitness,
    BackendError,
    UntilCancelled,
}

struct FixtureProvider {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    behavior: Behavior,
}

impl BackendProvider for FixtureProvider {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("resolved submissions must not prepare project images")
    }
    fn retained(
        &self,
        inputs: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
        Ok(Box::new(FixtureExecutor {
            profile: inputs.profile.clone(),
            calls: self.calls.clone(),
            entered: self.entered.clone(),
            behavior: self.behavior,
        }))
    }
}

struct FixtureExecutor {
    profile: ExecutionProfile,
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    behavior: Behavior,
}
impl ExecutionBackend for FixtureExecutor {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn execute(
        &self,
        request: ExecutionRequest<'_>,
    ) -> seedbox_application::Result<ExecutionReport> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.seed, 123);
        assert!(request.resume_checkpoint.is_none());
        assert!(request.output.is_dir());
        for (digest, path) in request.artifacts {
            assert_eq!(&ContentDigest::of(&std::fs::read(path)?), digest);
        }
        self.entered.notify_one();
        if matches!(self.behavior, Behavior::UntilCancelled) {
            while !request.cancellation.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(ExecutionReport {
            reason: match self.behavior {
                Behavior::MissingWitness => StopReason::Completed,
                Behavior::BackendError => StopReason::Error,
                Behavior::UntilCancelled => StopReason::Cancelled,
            },
            workload_exit: if matches!(self.behavior, Behavior::UntilCancelled) {
                None
            } else {
                Some(WorkloadExit {
                    service: "oracle".into(),
                    code: 0,
                })
            },
            detail: matches!(self.behavior, Behavior::BackendError)
                .then(|| "host fixture: interaction witness unavailable".into()),
            witness: None,
            full_state: None,
            faults: Vec::new(),
            host_wait_ns: 0,
            simulated_time_ns: Some(42),
        })
    }
}

struct HostService {
    _directory: tempfile::TempDir,
    client: Client,
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    cancellation: CancellationToken,
    server: tokio::task::JoinHandle<Result<(), seedbox_server::execution::Error>>,
    _cancel_on_drop: CancelOnDrop,
}
// A failed assertion must also release a waiting fixture executor.
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl HostService {
    async fn start(behavior: Behavior) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::new(directory.path().join("store"));
        // Match support::resolved_request's complete catalogue fixture closure.
        // These inert bytes are materialized for the host executor, never booted.
        store.put(b"catalogue-only-binary").unwrap();
        let manager = Arc::new(Manager::new(Catalogue::in_memory()));
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let service = Arc::new(
            ExecutionService::new(
                manager.clone(),
                directory.path().join("execution"),
                store,
                Arc::new(FixtureProvider {
                    calls: calls.clone(),
                    entered: entered.clone(),
                    behavior,
                }),
            )
            .unwrap(),
        );
        let cancellation = CancellationToken::default();
        let socket = directory.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(seedbox_server::serve(
            listener,
            manager.clone(),
            service,
            NonZeroUsize::new(1).unwrap(),
            cancellation.clone(),
        ));
        let client = Client::connect_to(&socket).unwrap();
        Self {
            _directory: directory,
            client,
            calls,
            entered,
            _cancel_on_drop: CancelOnDrop(cancellation.clone()),
            cancellation,
            server,
        }
    }
    async fn completed(&self, id: &seedbox_api::SimulationId) -> seedbox_api::Simulation {
        let mut stream = self.client.watch(id).await.unwrap();
        while let Some(resource) = stream.next().await {
            let resource = resource.unwrap();
            if resource.status.phase == Phase::Completed {
                return resource;
            }
        }
        panic!("watch closed before completion")
    }
    async fn shutdown(self) {
        self.cancellation.cancel();
        self.server.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn public_dispatch_retains_oracle_and_missing_witness_errors_without_comparability() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for (behavior, diagnostic) in [
            (
                Behavior::MissingWitness,
                "completed workload is missing its execution witness",
            ),
            (
                Behavior::BackendError,
                "host fixture: interaction witness unavailable",
            ),
        ] {
            let host = HostService::start(behavior).await;
            let request = support::resolved_request("host-execution");
            let accepted = host.client.submit(&request).await.unwrap();
            let id = &accepted.metadata.id;
            let completed = host.completed(id).await;
            assert_eq!(completed.status.oracle_exit_code, Some(0));
            assert_eq!(completed.status.verdict(), TestVerdict::Passed);
            assert!(matches!(&completed.status.termination,
                Some(ExecutionTermination::Error { message }) if message.contains(diagnostic)));
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
            // Idempotent transport retries never dispatch a second execution.
            assert_eq!(host.client.submit(&request).await.unwrap().metadata.id, *id);
            let comparison = host.client.compare(id, id).await.unwrap();
            assert_eq!(comparison.status, ComparisonStatus::NotComparable);
            assert_eq!(comparison.witness_matches, None);
            let before = host.client.describe(id).await.unwrap();
            let _ = host.client.world(id).await.unwrap();
            let _ = host.client.timeline(id).await.unwrap();
            let mut events = host.client.events_with(id, false).await.unwrap();
            let mut history = Vec::new();
            while let Some(delivery) = events.next().await {
                match delivery.unwrap() {
                    seedbox_api::EventDelivery::Event { observation, .. } => history.push(observation.event),
                    seedbox_api::EventDelivery::Closed => {},
                    seedbox_api::EventDelivery::Gap { .. } => panic!("unexpected host fixture event gap"),
                }
            }
            assert!(history.contains(&seedbox_api::EventKind::OracleExited { code: 0 }));
            assert!(history.contains(&seedbox_api::EventKind::ExecutionTerminated {
                termination: completed.status.termination.clone().unwrap(),
            }));
            let mut logs = host.client.logs_with(id, "oracle", false).await.unwrap();
            // There is intentionally no worker observation transport in this
            // fixture. Its absent final watermark must stay visible to readers.
            assert_eq!(logs.next().await.unwrap().unwrap(), seedbox_api::LogDelivery::Entry {
                sequence: 1, entry: seedbox_api::LogEntry::SourceUnavailable,
            });
            assert_eq!(logs.next().await.unwrap().unwrap(), seedbox_api::LogDelivery::Closed);
            assert!(logs.next().await.is_none());
            assert!(host.client.checkpoints(id).await.unwrap().is_empty());
            assert_eq!(host.client.describe(id).await.unwrap(), before);
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
            assert!(matches!(
                host.client.checkpoint(id, "uncaptured").await,
                Err(Error::Rejected {
                    code: ErrorCode::NotFound,
                    ..
                })
            ));
            assert!(matches!(
                host.client.checkpoint_definition(id, "uncaptured").await,
                Err(Error::Rejected {
                    code: ErrorCode::InvalidRequest,
                    message,
                }) if message == "invalid catalogue record: checkpoint uncaptured has not been captured"
            ));
            assert!(matches!(
                host.client
                    .request_control(
                        id,
                        &seedbox_api::ControlRequest {
                            request_id: "terminal-step".into(),
                            command: seedbox_api::ControlCommand::Step,
                        }
                    )
                    .await,
                Err(Error::Rejected {
                    code: ErrorCode::Conflict,
                    ..
                })
            ));
            assert_eq!(host.client.stop(id).await.unwrap().status, before.status);
            // Reusing the frozen public definition reruns the same host inputs;
            // absent witnesses remain NOT_COMPARABLE rather than becoming MATCH.
            let derived = host
                .client
                .derive(
                    id,
                    &seedbox_api::DeriveRequest {
                        request_id: "derive-host".into(),
                        source_created_at_unix_ms: before.metadata.created_at_unix_ms,
                        included: Vec::new(),
                        start_paused: false,
                    },
                )
                .await
                .unwrap();
            let repeated = host.completed(&derived.metadata.id).await;
            assert_eq!(repeated.status.verdict(), TestVerdict::Passed);
            assert_eq!(repeated.status.termination, completed.status.termination);
            assert_eq!(host.calls.load(Ordering::SeqCst), 2);
            assert_eq!(
                host.client
                    .compare(id, &derived.metadata.id)
                    .await
                    .unwrap()
                    .status,
                ComparisonStatus::NotComparable
            );
            host.client.delete(id).await.unwrap();
            // The derived definition owns its closure; removing its source must
            // not remove the descendant or erase its retained evidence error.
            assert_eq!(
                host.client
                    .describe(&derived.metadata.id)
                    .await
                    .unwrap()
                    .status,
                repeated.status
            );
            host.client.delete(&derived.metadata.id).await.unwrap();
            assert!(matches!(
                host.client.describe(id).await,
                Err(Error::Rejected {
                    code: ErrorCode::NotFound,
                    ..
                })
            ));
            host.shutdown().await;
        }
    })
    .await
    .expect("host service dispatch did not finish within five seconds");
}

#[tokio::test]
async fn public_stop_and_service_shutdown_cancel_dispatched_host_execution() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for shutdown in [false, true] {
            let host = HostService::start(Behavior::UntilCancelled).await;
            let accepted = host
                .client
                .submit(&support::resolved_request("cancel-host"))
                .await
                .unwrap();
            host.entered.notified().await;
            let control = seedbox_api::ControlRequest {
                request_id: "pending-step".into(),
                command: seedbox_api::ControlCommand::Step,
            };
            assert_eq!(
                host.client
                    .request_control(&accepted.metadata.id, &control)
                    .await
                    .unwrap()
                    .state,
                seedbox_api::ControlState::Accepted
            );
            // Subscribe before shutdown closes the listener. Graceful shutdown
            // must still deliver completion to an existing public observer.
            let mut observation = host.client.watch(&accepted.metadata.id).await.unwrap();
            if shutdown {
                host.cancellation.cancel();
            } else {
                host.client.stop(&accepted.metadata.id).await.unwrap();
            }
            let completed = loop {
                let resource = observation
                    .next()
                    .await
                    .expect("watch closed before completion")
                    .unwrap();
                if resource.status.phase == Phase::Completed {
                    break resource;
                }
            };
            assert!(observation.next().await.is_none());
            assert_eq!(
                completed.status.termination,
                Some(ExecutionTermination::Stopped)
            );
            assert_eq!(completed.status.verdict(), TestVerdict::Incomplete);
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
            if !shutdown {
                let receipt = host
                    .client
                    .control_receipt(&accepted.metadata.id, "pending-step")
                    .await
                    .unwrap();
                assert_eq!(receipt.state, seedbox_api::ControlState::Interrupted);
                assert_eq!(
                    host.client
                        .request_control(&accepted.metadata.id, &control)
                        .await
                        .unwrap(),
                    receipt
                );
                host.client.delete(&accepted.metadata.id).await.unwrap();
            }
            host.shutdown().await;
        }
    })
    .await
    .expect("host service cancellation did not finish within five seconds");
}
