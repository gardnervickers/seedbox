//! Independent retained data conversion before any native machine is created.
use super::{wire, MachineDefinition, Result};
use seedbox_driver::driver::SpawnDefinition;

/// Convert every retained machine in order: the oracle, the initial machines,
/// then the spawnable ones. The first invalid input fails the conversion.
pub(super) fn definitions(
    prepared: &wire::Prepared,
) -> Result<(
    MachineDefinition,
    Vec<MachineDefinition>,
    Vec<SpawnDefinition>,
)> {
    let mut converted = std::iter::once(&prepared.oracle)
        .chain(&prepared.machines)
        .chain(prepared.spawned.iter().map(|spawned| &spawned.machine))
        .map(super::definition)
        .collect::<Result<Vec<_>>>()?
        .into_iter();
    let oracle = converted.next().expect("converted oracle");
    let machines = converted.by_ref().take(prepared.machines.len()).collect();
    let spawned = converted
        .zip(&prepared.spawned)
        .map(|(definition, spawned)| SpawnDefinition {
            spec: spawned.spec.clone(),
            definition,
            allow_crashes: spawned.machine.allow_crashes,
        })
        .collect();
    Ok((oracle, machines, spawned))
}

#[cfg(test)]
mod tests;
