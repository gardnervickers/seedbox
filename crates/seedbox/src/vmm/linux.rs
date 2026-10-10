use super::*;
use crate::engine_adapter::{report, MACHINE_OUTPUT_DIR};
use crate::host::HostLease;
use std::{fs, os::unix::process::CommandExt};

pub(super) fn execute(
    backend: &VmmExecutor,
    request: ExecutionRequest<'_>,
) -> Result<ExecutionReport> {
    run_worker(
        backend,
        &request,
        &[],
        request.limits.wall_time_ns,
        |bytes| report(bytes, &request),
        |stopped| stopped,
    )
}

/// Boot once under the recorded boot seed and release each request's seed
/// from the barrier in one worker. The engine writes each seed's output
/// under `seed-N/`; each moves to its own request's output, with its own
/// report, so every attempt retains and replays like a lone run.
pub(super) fn execute_forked(
    backend: &VmmExecutor,
    requests: &[ExecutionRequest<'_>],
) -> Result<Vec<Result<ExecutionReport>>> {
    let (primary, rest) = requests
        .split_first()
        .ok_or_else(|| Error::Execution("empty forked execution".into()))?;
    for request in rest {
        backend.validate_retained(request.prepared)?;
        backend.preflight(Some(request.output))?;
        if request.prepared.profile != backend.profile {
            return Err(Error::Integrity("execution profile mismatch".into()));
        }
        fs::write(
            request.output.join("world.json"),
            serde_json::to_vec_pretty(&crate::engine_adapter::world(request)?)?,
        )?;
    }
    let seeds: Vec<u64> = rest.iter().map(|request| request.seed).collect();
    let wall_time_ns = primary
        .limits
        .wall_time_ns
        .saturating_mul(requests.len() as u64);
    run_worker(
        backend,
        primary,
        &seeds,
        wall_time_ns,
        |bytes| split_forked(bytes, requests),
        |stopped| requests.iter().map(|_| Ok(stopped.clone())).collect(),
    )
}

/// Move each seed's engine output to its own request and check its report.
fn split_forked(
    bytes: &[u8],
    requests: &[ExecutionRequest<'_>],
) -> Result<Vec<Result<ExecutionReport>>> {
    let batch: serde_json::Value = serde_json::from_slice(bytes)?;
    let reports = batch["seeds"]
        .as_array()
        .filter(|reports| reports.len() == requests.len())
        .ok_or_else(|| Error::Integrity("forked VMM report lacks one report per seed".into()))?;
    let primary = requests[0].output;
    let booted = primary.join("fork-boot");
    fs::rename(primary.join(MACHINE_OUTPUT_DIR), &booted)?;
    let stderr = primary.join("vmm.stderr");
    let mut results = Vec::new();
    for (index, (request, raw)) in requests.iter().zip(reports).enumerate() {
        results.push((|| {
            fs::rename(
                booted.join(format!("seed-{}", request.seed)),
                request.output.join(MACHINE_OUTPUT_DIR),
            )?;
            let bytes = serde_json::to_vec_pretty(raw)?;
            fs::write(request.output.join("vmm.json"), &bytes)?;
            if index > 0 {
                fs::copy(&stderr, request.output.join("vmm.stderr"))?;
            }
            fs::write(
                request.output.join("fork.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "boot": batch["boot"], "index": index,
                    "seeds": requests.iter().map(|r| r.seed).collect::<Vec<_>>(),
                }))?,
            )?;
            report(&bytes, request)
        })());
    }
    // What remains is the boot's own console prefix, which every seed's
    // console already starts with.
    fs::remove_dir_all(&booted)?;
    Ok(results)
}

fn run_worker<T>(
    backend: &VmmExecutor,
    request: &ExecutionRequest<'_>,
    fork_seeds: &[u64],
    wall_time_ns: u64,
    parse: impl FnOnce(&[u8]) -> Result<T>,
    stopped: impl FnOnce(ExecutionReport) -> T,
) -> Result<T> {
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
    use crate::worker_process::{incomplete, WorkerLaunch};
    // Performance values travel in the launch request; the environment stays cleared.
    let mut launch = WorkerLaunch::prepare(request, fork_seeds)?;

    if request.cancellation.is_cancelled() {
        return Ok(stopped(incomplete(StopReason::Cancelled)));
    }
    let static_evidence = backend
        .admission
        .lock()
        .map_err(|_| Error::Execution("host admission evidence lock poisoned".into()))?
        .clone();
    fs::write(
        request.output.join("host-static.json"),
        serde_json::to_vec_pretty(&static_evidence)?,
    )?;
    let (cpu, lease) = HostLease::acquire_checked(backend.cpu, |reservation| {
        let mut observed = Vec::new();
        let admission = crate::admission::collect_admitted(
            reservation.cpus.iter().copied(),
            &mut observed,
            crate::admission::observe,
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
    // before releasing the leased CPU.
    let child = launch.spawn()?;
    let completion = child.wait(request.cancellation, wall_time_ns, parse, stopped);
    drop(lease);
    completion
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Check, Limits, Network, Schedule};
    use serde_json::json;

    #[test]
    fn forked_output_moves_to_each_seed_with_its_own_report() {
        let digest = ContentDigest::of(b"retained input");
        let mut semantic_parameters = SemanticParameters::new(512);
        semantic_parameters.boot_seed = Some(10);
        let prepared = PreparedInputs {
            wall_clock_epoch: None,
            kernel: digest.clone(),
            images: BTreeMap::new(),
            placement: BTreeMap::from([("client".into(), "client".into())]),
            guests: BTreeMap::from([("client".into(), digest.clone())]),
            profile: ExecutionProfile {
                backend: Backend::Kvm,
                engine: digest.clone(),
                witness_mode: WitnessMode::Exits,
                semantic_parameters,
            },
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        let limits = Limits {
            simulated_time_ns: 1_000_000_000,
            wall_time_ns: 5_000_000_000,
        };
        let network = Network {
            links: vec![],
            name: "private".into(),
            latency_ns: 100_000,
            jitter_ns: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
        };
        let schedule = Schedule::default();
        let check = Check {
            service: "client".into(),
            exit_code: 0,
        };
        let artifacts = BTreeMap::new();
        let cancellation = CancellationToken::default();
        let performance = BTreeMap::new();
        let outputs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let requests: Vec<_> = [10, 11]
            .into_iter()
            .zip(&outputs)
            .map(|(seed, output)| ExecutionRequest {
                seed,
                limits: &limits,
                network: &network,
                schedule: &schedule,
                control: None,
                check: &check,
                prepared: &prepared,
                artifacts: &artifacts,
                output: output.path(),
                cancellation: &cancellation,
                performance_parameters: &performance,
            })
            .collect();
        let engine = outputs[0].path().join(MACHINE_OUTPUT_DIR);
        for seed in [10, 11] {
            fs::create_dir_all(engine.join(format!("seed-{seed}"))).unwrap();
            fs::write(
                engine.join(format!("seed-{seed}/client.console")),
                format!("seed {seed}"),
            )
            .unwrap();
        }
        fs::write(engine.join("client.console"), "boot").unwrap();
        fs::write(outputs[0].path().join("vmm.stderr"), "diagnostic").unwrap();
        let report = |seed: u64| json!({"report_version": 1, "seed": seed});
        let batch = json!({"report_version":1,"boot":{"seed":10},"seeds":[report(10), report(11)]});
        let results = split_forked(&serde_json::to_vec(&batch).unwrap(), &requests).unwrap();
        assert_eq!(results.len(), 2);
        for (seed, output) in [10_u64, 11].iter().zip(&outputs) {
            let path = output.path();
            assert_eq!(
                fs::read_to_string(path.join(MACHINE_OUTPUT_DIR).join("client.console")).unwrap(),
                format!("seed {seed}")
            );
            let retained: serde_json::Value =
                serde_json::from_slice(&fs::read(path.join("vmm.json")).unwrap()).unwrap();
            assert_eq!(retained["seed"], *seed);
            assert_eq!(
                fs::read_to_string(path.join("vmm.stderr")).unwrap(),
                "diagnostic"
            );
            let fork: serde_json::Value =
                serde_json::from_slice(&fs::read(path.join("fork.json")).unwrap()).unwrap();
            assert_eq!(fork["seeds"], json!([10, 11]));
        }
        assert!(!outputs[0].path().join("fork-boot").exists());
        // A report per seed is required.
        let short = json!({"report_version":1,"seeds":[report(10)]});
        assert!(split_forked(&serde_json::to_vec(&short).unwrap(), &requests).is_err());
    }
}
