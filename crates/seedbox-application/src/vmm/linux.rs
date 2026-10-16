use super::*;
use crate::engine_adapter::report;

pub(super) fn execute(
    backend: &VmmExecutor,
    request: ExecutionRequest<'_>,
) -> Result<ExecutionReport> {
    backend.validate_retained(request.prepared)?;
    backend.preflight(Some(request.output))?;
    if request.prepared.profile != backend.profile {
        return Err(Error::Integrity("execution profile mismatch".into()));
    }
    use crate::worker_process::{incomplete, WorkerLaunch};
    // Performance values travel in the launch request; the environment stays cleared.
    let launch = WorkerLaunch::prepare(&request)?;

    if request.cancellation.is_cancelled() {
        return Ok(incomplete(StopReason::Cancelled));
    }
    let child = launch.spawn_worker()?;
    child.wait(request.cancellation, request.limits.wall_time_ns, |bytes| {
        report(bytes, &request)
    })
}
