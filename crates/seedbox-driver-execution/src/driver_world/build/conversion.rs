//! Independent retained data conversion before any native machine is created.
use super::{wire, Error, MachineDefinition, Result};
use std::{panic::AssertUnwindSafe, thread};

pub(super) fn definitions(
    prepared: &wire::Prepared,
    workers: usize,
) -> Result<(
    MachineDefinition,
    Vec<MachineDefinition>,
    Vec<MachineDefinition>,
)> {
    let inputs: Vec<_> = std::iter::once(&prepared.oracle)
        .chain(&prepared.machines)
        .chain(&prepared.spawnable)
        .collect();
    let mut converted = convert(&inputs, workers, &super::definition)?.into_iter();
    let oracle = converted.next().expect("converted oracle");
    let machines = converted.by_ref().take(prepared.machines.len()).collect();
    Ok((oracle, machines, converted.collect()))
}

fn convert<T: Send>(
    inputs: &[&wire::Machine],
    workers: usize,
    read: &(impl Fn(&wire::Machine) -> Result<T> + Sync),
) -> Result<Vec<T>> {
    let workers = workers.min(inputs.len());
    if workers <= 1 {
        return inputs.iter().map(|input| read(input)).collect();
    }
    thread::scope(|scope| {
        let mut jobs = Vec::with_capacity(workers);
        let mut failure = None;
        for worker in 0..workers {
            let job = thread::Builder::new()
                .name(format!("seedbox-definition-{worker}"))
                .spawn_scoped(scope, move || {
                    (worker..inputs.len())
                        .step_by(workers)
                        .map(|index| {
                            let result =
                                std::panic::catch_unwind(AssertUnwindSafe(|| read(inputs[index])))
                                    .unwrap_or_else(|_| {
                                        Err(Error::Execution(format!(
                                            "Driver definition conversion for {} panicked",
                                            inputs[index].name
                                        )))
                                    });
                            (index, result)
                        })
                        .collect::<Vec<_>>()
                });
            match job {
                Ok(job) => jobs.push(job),
                Err(error) => {
                    failure = Some(error.into());
                    break;
                }
            }
        }
        let mut results = Vec::with_capacity(inputs.len());
        for job in jobs {
            match job.join() {
                Ok(converted) => results.extend(converted),
                Err(_) if failure.is_none() => {
                    failure = Some(Error::Execution(
                        "Driver definition conversion worker panicked".into(),
                    ));
                }
                Err(_) => {}
            }
        }
        // Join every started worker, even on panic or spawn failure. Keep each
        // occurrence and select ordinary conversion errors in original order.
        if let Some(error) = failure {
            return Err(error);
        }
        results.sort_by_key(|(index, _)| *index);
        results.into_iter().map(|(_, result)| result).collect()
    })
}

#[cfg(test)]
mod tests;
