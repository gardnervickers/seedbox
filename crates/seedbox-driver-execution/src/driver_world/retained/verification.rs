//! Bounded host verification before a retained worker request can be used.
use super::{Artifact, Error, Result};
use std::{panic::AssertUnwindSafe, thread};

pub(super) fn artifacts(artifacts: &[Artifact], workers: usize) -> Result<()> {
    verify(artifacts, workers, &|artifact| {
        super::super::checkpoint::verified(artifact).map(|_| ())
    })
}

fn verify(
    artifacts: &[Artifact],
    workers: usize,
    read: &(impl Fn(&Artifact) -> Result<()> + Sync),
) -> Result<()> {
    let workers = workers.min(artifacts.len());
    if workers <= 1 {
        for artifact in artifacts {
            read(artifact)?;
        }
        return Ok(());
    }
    thread::scope(|scope| {
        let mut jobs = Vec::with_capacity(workers);
        let mut failure = None;
        for worker in 0..workers {
            let job = thread::Builder::new()
                .name(format!("seedbox-verify-{worker}"))
                .spawn_scoped(scope, move || {
                    (worker..artifacts.len())
                        .step_by(workers)
                        .map(|index| {
                            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                                read(&artifacts[index])
                            }))
                            .unwrap_or_else(|_| {
                                Err(Error::Execution(
                                    "Driver artifact verification panicked".into(),
                                ))
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
        let mut results = Vec::with_capacity(artifacts.len());
        for job in jobs {
            match job.join() {
                Ok(reads) => results.extend(reads),
                Err(_) if failure.is_none() => {
                    failure = Some(Error::Execution(
                        "Driver artifact verification worker panicked".into(),
                    ));
                }
                Err(_) => {}
            }
        }
        // Every started verifier is joined, including on failures. Completion
        // order cannot select which artifact's integrity error is reported.
        if let Some(error) = failure {
            return Err(error);
        }
        results.sort_by_key(|(index, _)| *index);
        for (_, result) in results {
            result?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests;
