//! Portable checkpoint files. Evidence I/O and deletion share a lock; no
//! archive field is ever used to derive a host path or launch work on import.
use super::*;
use seedbox_artifacts::archive::{self, Options};
use std::io::Read;

impl ExecutionService {
    /// Export an independent resumable checkpoint, including every binary input.
    pub fn export_checkpoint_bundle(
        &self,
        id: &SimulationId,
        checkpoint: &str,
        output: impl Write,
        options: &Options,
    ) -> Result<(), Error> {
        let _guard = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let resource = self.checkpoint_definition_locked(id, checkpoint, &mut |_| {
            if options.cancellation.is_cancelled() {
                Err(std::io::Error::other("checkpoint export cancelled"))
            } else {
                Ok(())
            }
        })?;
        // The public spec serializer folds the authoritative input program
        // (including recorded external controls) into the resolved definition.
        // Reading spec.inputs directly would lose those timeline edits.
        let definition: seedbox_spec::definition::Definition = serde_json::from_value(
            serde_json::to_value(resource.spec).map_err(|e| Error::Bundle(e.to_string()))?,
        )
        .map_err(|e| Error::Bundle(e.to_string()))?;
        archive::export_checkpoint(&definition, &self.store, output, options)
            .map_err(|e| Error::Bundle(e.to_string()))
    }

    /// Restore complete binary inputs without registering or launching a simulation.
    pub fn import_checkpoint_bundle(
        &self,
        input: impl Read,
        options: &Options,
    ) -> Result<seedbox_spec::definition::Definition, Error> {
        // Its owned binaries stay pinned until a submission names them.
        let _references = self
            .references
            .read()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let definition = archive::import_checkpoint(input, &self.store, options)
            .map_err(|e| Error::Bundle(e.to_string()))?;
        seedbox_application::definition::prepared(&definition, &self.store)
            .map_err(|e| Error::Bundle(e.to_string()))?;
        self.pin_upload(&definition)?;
        Ok(definition)
    }

    pub(super) fn portable_simulation(
        &self,
        id: &SimulationId,
    ) -> seedbox_application::Result<seedbox_api::Simulation> {
        let simulation = self
            .describe(id)
            .map_err(|e| seedbox_application::Error::Setup(e.to_string()))?;
        let mut simulation: seedbox_api::Simulation =
            serde_json::from_value(serde_json::to_value(simulation)?)?;
        let path = self.root.join("inputs").join(format!("{id}.json"));
        match fs::File::open(path) {
            Ok(file) => {
                let inputs: RetainedInputs = serde_json::from_reader(file)?;
                if let (Some(path), InputSource::Resolved { definition }) =
                    (&inputs.checkpoint, &mut simulation.spec.inputs)
                {
                    definition.start = seedbox_application::definition::capture_start(
                        path,
                        &self.root.join("runs"),
                        &self.store,
                    )?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(simulation)
    }
}

/// Ignore only provenance at its owning schema paths. Actual architecture,
/// backend, launch kind/ownership/observation contract, policies, binaries and
/// checkpoint state remain comparable execution inputs.
pub(super) fn semantic_definition(
    mut definition: Box<seedbox_spec::definition::Definition>,
) -> seedbox_application::Result<Box<seedbox_spec::definition::Definition>> {
    for machine in definition
        .machines
        .values_mut()
        .chain(std::iter::once(&mut definition.oracle))
    {
        machine.image = None;
    }
    if let Some(value) = definition
        .runtime
        .execution
        .pointer_mut("/semantic_parameters/engine")
        .filter(|value| !value.is_null())
    {
        let mut descriptor: seedbox_artifacts::execution::EngineDescriptor =
            serde_json::from_value(value.clone())?;
        descriptor.simulator_version.clear();
        descriptor.host_os.clear();
        descriptor.host_arch.clear();
        descriptor.guest_arch.clear();
        descriptor.engine_timebase.clear();
        *value = serde_json::to_value(descriptor)?;
    }
    Ok(definition)
}
