use super::*;
use crate::host::HostLease;
use std::{fs, os::unix::process::CommandExt};

pub(super) fn execute(
    backend: &VmmExecutor,
    request: ExecutionRequest<'_>,
) -> Result<ExecutionReport> {
    backend.validate_retained(request.prepared)?;
    backend.preflight(Some(request.output))?;
    if request.prepared.profile != backend.profile {
        return Err(Error::Integrity("execution profile mismatch".into()));
    }
    if backend
        .cpu
        .is_some_and(|cpu| cpu >= libc::CPU_SETSIZE as u32)
    {
        return Err(Error::Execution("CPU exceeds affinity mask".into()));
    }
    let timebase = recorded_timebase(&backend.profile)?;
    use crate::worker_process::{incomplete, WorkerLaunch};
    // Performance values travel in the launch request; the environment stays cleared.
    let mut launch = WorkerLaunch::prepare(&request)?;

    if request.cancellation.is_cancelled() {
        return Ok(incomplete(
            StopReason::Cancelled,
            recorded_timebase(&backend.profile)?,
        ));
    }
    let static_evidence = backend
        .admission
        .lock()
        .map_err(|_| Error::Execution("host admission evidence lock poisoned".into()))?
        .clone();
    let counted = timebase == Timebase::Counter;
    let workaround = backend.counter.workaround().filter(|_| counted);
    if counted {
        fs::write(
            request.output.join("host-static.json"),
            serde_json::to_vec_pretty(&static_evidence)?,
        )?;
    }
    let (cpu, mut lease) =
        HostLease::acquire_checked(backend.cpu, workaround, request.output, |reservation| {
            if !counted {
                return Ok(());
            }
            let mut observed = Vec::new();
            let admission = crate::admission::collect_admitted(
                reservation.cpus.iter().copied(),
                &mut observed,
                |cpu| crate::admission::observe_counter(cpu, backend.counter),
            );
            crate::admission::finish_admission(admission, |admission| {
                fs::write(
                    request.output.join("host-leased.json"),
                    serde_json::to_vec_pretty(&observed)?,
                )?;
                if let Err(error) = admission {
                    fs::write(
                        request.output.join("host-leased-error.txt"),
                        error.to_string(),
                    )?;
                }
                Ok(())
            })
        })?;
    let originals = lease.original_registers();
    if workaround.is_some() {
        fs::write(
            request.output.join("host-workaround.json"),
            serde_json::to_vec_pretty(&lease.enabled_registers()?)?,
        )?;
    }
    unsafe {
        launch.command.pre_exec(move || {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(cpu as usize, &mut set);
            if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Declared after the lease: every error path finishes process cleanup
    // before releasing the leased CPU or restoring MSRs.
    let child = launch.spawn()?;
    let completion = child.wait(request.cancellation, request.limits.wall_time_ns, |bytes| {
        report(bytes, &request)
    });
    lease.restore()?;
    if workaround.is_some() {
        fs::write(
            request.output.join("host-restored.json"),
            serde_json::to_vec_pretty(&originals)?,
        )?;
    }
    completion
}
