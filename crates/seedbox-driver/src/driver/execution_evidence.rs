//! Specific guarded native witness read, not arbitrary mutable inspection.
use super::{Barrier, Error, Machine, MachineId};
use seedbox_machine::machine::ExecutionEvidence;
impl<M> Barrier<'_, M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Observe native interaction evidence at a stopped barrier. Finished and
    /// permanently stopped drivers are allowed; poisoned/in-flight instances
    /// are rejected before any backend operation. This never grants run/RNG/
    /// clock access or forces full RAM collection. Required None is a caller
    /// diagnostic, not a successful normal native witness.
    pub fn execution_evidence(
        &mut self,
        id: MachineId,
    ) -> Result<Option<ExecutionEvidence>, Error<M::Error>> {
        self.driver.check_valid()?;
        self.driver
            .execution
            .machines
            .get_mut(id.0)
            .ok_or(Error::UnknownMachine(id))?
            .machine
            .as_mut()
            .ok_or(Error::Failed)?
            .execution_evidence()
            .map_err(|source| Error::Machine { id, source })
    }
}

/// One stopped native evidence read. Results retain machine identity even when
/// another machine fails, so consumers can validate them in insertion order.
#[derive(Debug)]
pub struct ExecutionEvidenceRead<E> {
    pub machine: MachineId,
    pub result: Result<Option<ExecutionEvidence>, Error<E>>,
}

impl<M> Barrier<'_, M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Read all stopped machines on the existing execution workers. Every
    /// accepted read finishes and returns its machine before this call returns.
    /// Reading evidence does not execute machines or change Driver policy.
    /// Per-machine failures remain in insertion order alongside successful reads.
    pub fn execution_evidence_all(
        &mut self,
    ) -> Result<Vec<ExecutionEvidenceRead<M::Error>>, Error<M::Error>> {
        self.driver.check_valid()?;
        let (send, receive) = std::sync::mpsc::channel();
        let mut submission_error = None;
        for (index, entry) in self.driver.execution.machines.iter_mut().enumerate() {
            let id = MachineId(index);
            let mut machine = entry.machine.take().expect("stopped machine");
            let result = send.clone();
            if let Err(error) = self.driver.workers.submit(index, move |_| {
                let evidence = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    machine.execution_evidence()
                }))
                .map_err(|_| Error::Panicked { id })
                .and_then(|read| read.map_err(|source| Error::Machine { id, source }));
                let _ = result.send((id, machine, evidence));
            }) {
                submission_error = Some(error);
                break;
            }
        }
        drop(send);
        let mut results: Vec<_> = receive.into_iter().collect();
        results.sort_by_key(|(id, _, _)| *id);
        let mut reads = Vec::with_capacity(results.len());
        for (id, machine, result) in results {
            self.driver.execution.machines[id.0].machine = Some(machine);
            reads.push(ExecutionEvidenceRead {
                machine: id,
                result,
            });
        }
        if let Some(error) = submission_error {
            return Err(error);
        }
        Ok(reads)
    }
}

#[cfg(test)]
mod tests;
