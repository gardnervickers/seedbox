//! Service-owned retained Driver execution state.
//!
//! Native factories and machine state have concrete private variants. The
//! service envelope retains already-published history and bootstrap authority,
//! while the Driver owns all scheduling and simulation decisions.
mod actions;
mod doctor;
pub use doctor::{doctor, DoctorReport};
pub(crate) mod binding;
mod boot;
mod build;
mod checkpoint;
mod events;
mod executor;
mod inventory;
mod results;
pub use executor::DriverExecutor;
pub use seedbox_driver::driver::faults::{action_targets, ActionTargets};
mod controller;
mod native;
pub mod platform;
mod policy;
pub mod preflight;
pub mod preparation;
pub mod retained;
pub use retained::retain_prepared;
pub use retained::runtime_digest;
mod transport;
mod volumes;
mod worker;
pub use volumes::{volume_setup, VolumeSetup};

// Parent-owned beneath the execution's machine output. A killed worker cannot
// run TempDir destructors; the monitor join owns removal of this subtree only.
pub(crate) const NATIVE_SCRATCH: &str = "native-scratch";

/// Execute one explicit retained Driver request in its isolated worker process.
/// Inputs are already materialized; this does no argument/environment discovery,
/// image resolution or building. Native state stays stopped between Driver
/// calls. Full-state requests require real backend evidence; missing evidence
/// becomes an error report, never fabricated state. The report does not treat
/// event publication hashes as execution witnesses.
// TODO: Re-evaluate: a retained executable owns native processes and Driver;
// moving this execution into the catalogue would change resource authority.
pub fn run_request(
    request: &seedbox_engine_protocol::driver_world::Request,
) -> crate::Result<crate::execution::ExecutionReport> {
    use crate::execution::StopReason;
    let mut worker = build::open(request)?;
    let names = &worker.names;
    let associated = worker
        .driver
        .with_barrier(|barrier| {
            Ok(barrier
                .machines()
                .all(|(id, machine)| names.get(&id).is_some_and(|name| name == machine.name())))
        })
        .map_err(|error| crate::Error::Execution(error.to_string()))?;
    if !associated {
        return Err(crate::Error::Integrity(
            "Driver/native service identities disagree".into(),
        ));
    }
    let mut control = transport::Control::bind(&request.host_control).map_err(|error| {
        crate::Error::Execution(format!(
            "Driver control socket {}: {error}",
            request.host_control.display()
        ))
    })?;
    let (mut reason, mut detail) = match worker.execute(request.simulated_time_ns, &mut control) {
        Ok(reason) => (reason, None),
        Err(error) => (StopReason::Error, Some(error.to_string())),
    };
    let mut failure = |error: crate::Error| {
        reason = StopReason::Error;
        detail = Some(match detail.take() {
            Some(previous) => format!("{previous}; {error}"),
            None => error.to_string(),
        });
    };
    let policy = match results::record_policy(&mut worker.driver, &request.output) {
        Ok(policy) => Some(policy),
        Err(error) => {
            failure(error);
            None
        }
    };
    let state = if request.full_state {
        match results::publish_state(&mut worker.driver, &worker.names, &request.output) {
            Ok(state) => Some(state),
            Err(error) => {
                failure(error);
                None
            }
        }
    } else {
        None
    };
    let witness = if let Some(policy) = policy.filter(|_| !request.full_state || state.is_some()) {
        match results::execution_witness(&mut worker.driver, &worker.names, &policy, state.as_ref())
        {
            Ok(witness) => Some(witness),
            Err(error) => {
                failure(error);
                None
            }
        }
    } else {
        None
    };
    let full_state = state.map(|state| {
        state
            .machines
            .into_iter()
            .map(|(name, capture)| (name, capture.digest))
            .collect()
    });
    let mut report = worker.report(reason, detail, witness, full_state)?;
    if let Err(error) = worker.close() {
        report.reason = StopReason::Error;
        report.detail = Some(match report.detail {
            Some(previous) => format!("{previous}; observation close: {error}"),
            None => format!("observation close: {error}"),
        });
    }
    Ok(report)
}
