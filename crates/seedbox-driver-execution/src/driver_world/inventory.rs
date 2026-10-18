//! One typed walk of every external future-factory artifact. Preparation and
//! checkpoint publication use the same closure boundary, including config data.
use crate::Result;
use seedbox_engine_protocol::driver_world::{Artifact, Inventory};

pub(super) fn visit(
    inventory: &mut Inventory,
    mut apply: impl FnMut(&mut Artifact) -> Result<()>,
) -> Result<()> {
    let (artifacts, formatter, environments, boots) = match inventory {
        Inventory::Pv {
            executable,
            kernel,
            init,
            command_line,
            progress,
            runtime,
            formatter,
            environments,
            boot_archives,
            ..
        } => (
            vec![executable, kernel, init, command_line, progress, runtime],
            formatter,
            environments,
            boot_archives,
        ),
        Inventory::Tcg {
            executable,
            kernel,
            init,
            command_line,
            firmware,
            runtime,
            formatter,
            environments,
            boot_archives,
            ..
        } => (
            vec![executable, kernel, init, command_line, firmware, runtime],
            formatter,
            environments,
            boot_archives,
        ),
    };
    for artifact in boots.values_mut() {
        apply(artifact)?;
    }
    for artifact in artifacts {
        apply(artifact)?;
    }
    if let Some(formatter) = formatter {
        apply(formatter)?;
    }
    for environment in environments.values_mut() {
        for artifact in environment.configs.values_mut() {
            apply(artifact)?;
        }
    }
    Ok(())
}
