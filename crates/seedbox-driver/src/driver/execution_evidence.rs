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
#[cfg(test)]
mod tests;
