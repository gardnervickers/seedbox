//! Pure argument and target compilation, shared by preview and executing admission.
use super::*;

pub(crate) struct CompiledTargets<M> {
    pub machine: Option<M>,
    pub sides: Option<(Vec<String>, Vec<String>)>,
    pub connection: Option<(String, String)>,
}

pub(crate) fn sides(
    left: &[Target],
    right: &[Target],
    resolve: impl Fn(&Target) -> Result<String, OperationError>,
) -> Result<(Vec<String>, Vec<String>), OperationError> {
    if left.is_empty() || right.is_empty() {
        return Err(OperationError::invalid("partition sides must be nonempty"));
    }
    let resolve_side = |side: &[Target]| side.iter().map(&resolve).collect::<Result<Vec<_>, _>>();
    let (mut left, mut right) = (resolve_side(left)?, resolve_side(right)?);
    let mut all: Vec<_> = left.iter().chain(&right).collect();
    all.sort();
    if all.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(OperationError::invalid(
            "partition sides must be disjoint with unique machines",
        ));
    }
    left.sort();
    right.sort();
    Ok((left, right))
}

impl Command {
    /// Resolves only names and argument shape. No IDs, handles, state checks or draws.
    /// The execution owner supplies Facts; observation-only callers supply names.
    pub(crate) fn compile_targets<M>(
        &self,
        machine: impl Fn(&Target) -> Result<M, OperationError>,
        network: impl Fn(&Target) -> Result<String, OperationError>,
        floor: impl Fn() -> Result<u64, OperationError>,
    ) -> Result<CompiledTargets<M>, OperationError> {
        let mut compiled = CompiledTargets {
            machine: None,
            sides: None,
            connection: None,
        };
        match self {
            Self::Named { command, .. } => return command.compile_targets(machine, network, floor),
            Self::Effect { target, .. }
            | Self::Throttle { target, .. }
            | Self::Drift { target, .. }
            | Self::StepRealtime { target, .. } => compiled.machine = Some(machine(target)?),
            Self::Partition { left, right } => compiled.sides = Some(sides(left, right, network)?),
            Self::Shape {
                from,
                to,
                policy,
                network_faults,
            } => {
                policy
                    .validate_shape(floor()?, network_faults)
                    .map_err(OperationError::invalid)?;
                compiled.sides = Some(sides(from, to, network)?);
            }
            Self::DropConnection { connection } => {
                Operation::DropConnection {
                    connection: connection.clone(),
                }
                .validate()?;
                let from = network(&Target::Machine(connection.from.clone()))?;
                let to = network(&Target::Machine(connection.to.clone()))?;
                if from == to {
                    return Err(OperationError::invalid("connection endpoints must differ"));
                }
                compiled.connection = Some((from, to));
            }
            Self::Heal { .. } | Self::HealRecord { .. } | Self::Spawn { .. } => {}
        }
        Ok(compiled)
    }
}
