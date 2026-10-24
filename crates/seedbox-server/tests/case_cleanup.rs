//! Portable control-flow proof for the shared native case boundary. These are
//! gated cleanup callbacks, not native-process or witness acceptance tests.
use case_cleanup::finish_before_rethrow;
use futures_util::FutureExt;
use seedbox_server::test_support::case_cleanup;
use std::{
    future::pending,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

type Events = Arc<Mutex<Vec<&'static str>>>;

async fn hang_guard(future: impl std::future::Future<Output = ()>) {
    // Infrastructure bound only. Correctness depends on explicit event gates.
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .unwrap();
}

struct Owner {
    events: Events,
    cleanup_entered: Option<oneshot::Sender<()>>,
    release_cleanup: Option<oneshot::Receiver<()>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.events.lock().unwrap().push("owner dropped");
    }
}
impl Owner {
    async fn cleanup(&mut self) -> Result<(), String> {
        self.events.lock().unwrap().push("cancel all");
        self.cleanup_entered.take().unwrap().send(()).unwrap();
        self.release_cleanup.take().unwrap().await.unwrap();
        self.events
            .lock()
            .unwrap()
            .extend(["blocking workers joined", "W2 verified"]);
        Ok(())
    }
}
#[derive(Debug, PartialEq)]
struct OriginalPanic;

async fn exercise(timeout: bool) {
    let events = Events::default();
    let (entered_send, cleanup_entered) = oneshot::channel();
    let (release, release_cleanup) = oneshot::channel();
    let (body_started_send, body_started) = oneshot::channel();
    let (deadline_send, deadline) = oneshot::channel();
    let owner = Owner {
        events: events.clone(),
        cleanup_entered: Some(entered_send),
        release_cleanup: Some(release_cleanup),
    };
    let task = tokio::spawn(
        AssertUnwindSafe(finish_before_rethrow(
            owner,
            move |owner| {
                Box::pin(async move {
                    owner.events.lock().unwrap().push("body entered");
                    body_started_send.send(()).unwrap();
                    if timeout {
                        pending::<()>().await;
                    } else {
                        std::panic::panic_any(OriginalPanic);
                    }
                })
            },
            |owner| Box::pin(owner.cleanup()),
            async move {
                deadline.await.unwrap();
            },
        ))
        .catch_unwind(),
    );
    body_started.await.unwrap();
    if timeout {
        deadline_send.send(()).unwrap();
    }
    cleanup_entered.await.unwrap();
    // This is a causal non-completion assertion, not an elapsed-time oracle:
    // cleanup is known entered and cannot proceed until this explicit permit.
    assert!(!task.is_finished(), "rethrow escaped the held cleanup gate");
    assert_eq!(*events.lock().unwrap(), ["body entered", "cancel all"]);
    release.send(()).unwrap();
    let result = task.await.unwrap();
    let payload = match result {
        Err(payload) => payload,
        Ok(_) => panic!("case failure was swallowed"),
    };
    if timeout {
        let text = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(text.contains("hang guard elapsed") && text.contains("cleanup verified"));
    } else {
        assert_eq!(
            payload.downcast_ref::<OriginalPanic>(),
            Some(&OriginalPanic)
        );
    }
    assert_eq!(
        *events.lock().unwrap(),
        [
            "body entered",
            "cancel all",
            "blocking workers joined",
            "W2 verified",
            "owner dropped"
        ]
    );
}
#[tokio::test]
async fn panic_rethrows_original_payload_only_after_awaited_cleanup() {
    hang_guard(async {
        exercise(false).await;
    })
    .await;
}
#[tokio::test]
async fn signalled_deadline_waits_for_cleanup_before_reporting_timeout() {
    hang_guard(async {
        exercise(true).await;
    })
    .await;
}
#[tokio::test]
async fn success_returns_owner_only_after_cleanup() {
    hang_guard(async {
        let events = Events::default();
        let (entered_send, entered) = oneshot::channel();
        let (release, receiver) = oneshot::channel();
        let task = tokio::spawn(finish_before_rethrow(
            Owner {
                events: events.clone(),
                cleanup_entered: Some(entered_send),
                release_cleanup: Some(receiver),
            },
            |owner| {
                Box::pin(async move {
                    owner.events.lock().unwrap().push("body entered");
                })
            },
            |owner| Box::pin(owner.cleanup()),
            pending(),
        ));
        entered.await.unwrap();
        assert!(!task.is_finished());
        release.send(()).unwrap();
        let owner = task.await.unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [
                "body entered",
                "cancel all",
                "blocking workers joined",
                "W2 verified"
            ]
        );
        drop(owner);
        assert_eq!(events.lock().unwrap().last(), Some(&"owner dropped"));
    })
    .await;
}
#[tokio::test]
async fn failed_cleanup_reports_both_failures_without_claiming_w2() {
    hang_guard(async {
        let events = Events::default();
        let (entered, _) = oneshot::channel();
        let (_, release) = oneshot::channel();
        let result = AssertUnwindSafe(finish_before_rethrow(
            Owner {
                events: events.clone(),
                cleanup_entered: Some(entered),
                release_cleanup: Some(release),
            },
            |_| {
                Box::pin(async {
                    panic!("original case marker");
                })
            },
            |owner| {
                Box::pin(async move {
                    owner.events.lock().unwrap().push("cancel all");
                    Err("missing cleanup receipt".into())
                })
            },
            pending(),
        ))
        .catch_unwind()
        .await;
        let payload = match result {
            Err(payload) => payload,
            Ok(_) => panic!("cleanup failure was swallowed"),
        };
        let text = payload.downcast_ref::<String>().unwrap();
        assert!(
            text.contains("missing cleanup receipt")
                && text.contains("original case marker")
                && text.contains("preserved")
        );
        assert_eq!(*events.lock().unwrap(), ["cancel all", "owner dropped"]);
    })
    .await;
}

#[tokio::test]
async fn body_factory_panic_still_runs_cleanup_before_original_rethrow() {
    hang_guard(async {
        let events = Events::default();
        let (entered_send, entered) = oneshot::channel();
        let (release, receiver) = oneshot::channel();
        release.send(()).unwrap();
        let result = AssertUnwindSafe(finish_before_rethrow(
            Owner {
                events: events.clone(),
                cleanup_entered: Some(entered_send),
                release_cleanup: Some(receiver),
            },
            |owner| {
                owner.events.lock().unwrap().push("body factory entered");
                std::panic::panic_any(OriginalPanic);
            },
            |owner| Box::pin(owner.cleanup()),
            pending(),
        ))
        .catch_unwind()
        .await;
        entered.await.unwrap();
        let payload = match result {
            Err(payload) => payload,
            Ok(_) => panic!("factory panic swallowed"),
        };
        assert_eq!(
            payload.downcast_ref::<OriginalPanic>(),
            Some(&OriginalPanic)
        );
        assert_eq!(
            *events.lock().unwrap(),
            [
                "body factory entered",
                "cancel all",
                "blocking workers joined",
                "W2 verified",
                "owner dropped"
            ]
        );
    })
    .await;
}
#[tokio::test]
async fn cleanup_factory_panic_preserves_original_failure_context() {
    hang_guard(async {
        let events = Events::default();
        let (entered, _) = oneshot::channel();
        let (_, release) = oneshot::channel();
        let result = AssertUnwindSafe(finish_before_rethrow(
            Owner {
                events: events.clone(),
                cleanup_entered: Some(entered),
                release_cleanup: Some(release),
            },
            |_| {
                Box::pin(async {
                    panic!("original factory case marker");
                })
            },
            |_| panic!("cleanup factory marker"),
            pending(),
        ))
        .catch_unwind()
        .await;
        let payload = match result {
            Err(payload) => payload,
            Ok(_) => panic!("cleanup factory panic swallowed"),
        };
        let text = payload.downcast_ref::<String>().unwrap();
        assert!(
            text.contains("original factory case marker")
                && text.contains("cleanup factory marker")
        );
        assert_eq!(*events.lock().unwrap(), ["owner dropped"]);
    })
    .await;
}
