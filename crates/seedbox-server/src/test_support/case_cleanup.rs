//! A case owns its resources outside the fallible/timed body. Cleanup is awaited
//! before a panic payload is resumed or a deadline is reported. Test-only code.
use futures_util::{future::BoxFuture, FutureExt};
use std::{any::Any, future::Future, panic::AssertUnwindSafe};

pub const DEADLINE_FAILURE: &str =
    "case hang guard elapsed; cancellation, joins and cleanup verified before rethrow";

pub fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload")
}

/// The deadline is injectable: native callers supply their existing hang guard;
/// portable regressions supply an explicitly signalled future, not a host sleep.
/// On success return the owner so the caller can remove verified inputs. On
/// failure its ordinary preserve-on-drop policy remains in force.
pub async fn finish_before_rethrow<S, Body, Cleanup, Deadline>(
    mut owner: S,
    body: Body,
    cleanup: Cleanup,
    deadline: Deadline,
) -> S
where
    Body: for<'a> FnOnce(&'a mut S) -> BoxFuture<'a, ()>,
    Cleanup: for<'a> FnOnce(&'a mut S) -> BoxFuture<'a, Result<(), String>>,
    Deadline: Future<Output = ()>,
{
    let outcome = AssertUnwindSafe(async {
        // Include invoking the body factory, not just polling its future.
        let body = body(&mut owner);
        tokio::select! {
            biased;
            () = body => true,
            () = deadline => false,
        }
    })
    .catch_unwind()
    .await;
    let cleaned = AssertUnwindSafe(async { cleanup(&mut owner).await })
        .catch_unwind()
        .await;
    let cleanup_error = match cleaned {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(payload) => Some(format!(
            "cleanup panicked: {}",
            panic_text(payload.as_ref())
        )),
    };
    if let Some(error) = cleanup_error {
        let original = match &outcome {
            Ok(true) => "case completed",
            Ok(false) => "case hang guard elapsed",
            Err(payload) => panic_text(payload.as_ref()),
        };
        panic!("case cleanup failed; uncertain inputs must be preserved: {error}; original case: {original}");
    }
    match outcome {
        Ok(true) => owner,
        Ok(false) => panic!("{DEADLINE_FAILURE}"),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}
