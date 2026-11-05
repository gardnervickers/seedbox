//! Prepare the machines an oracle spawns during a project's run. Preparation
//! backends are resolved at the first spawn, so runs that never spawn pay
//! nothing for them.
use super::BackendProvider;
use seedbox_application::{
    config::Simulation,
    execution::{CancellationToken, PreparationBackend, Spawner},
    store::ContentStore,
    Error, Result,
};
use seedbox_engine_protocol::driver_world as wire;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub(super) struct ProjectSpawner {
    backends: Arc<dyn BackendProvider>,
    configuration: Simulation,
    backend: seedbox_spec::backend::CpuBackend,
    store: ContentStore,
    cancellation: CancellationToken,
    preparation: Mutex<Option<Box<dyn PreparationBackend>>>,
}
impl ProjectSpawner {
    pub(super) fn new(
        backends: Arc<dyn BackendProvider>,
        configuration: Simulation,
        backend: seedbox_spec::backend::CpuBackend,
        store: ContentStore,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            backends,
            configuration,
            backend,
            store,
            cancellation,
            preparation: Mutex::new(None),
        }
    }
}
impl Spawner for ProjectSpawner {
    fn prepare(
        &self,
        request: &wire::SpawnRequest,
        directory: &Path,
    ) -> Result<(wire::Machine, wire::Environment)> {
        let mut preparation = self
            .preparation
            .lock()
            .map_err(|_| Error::Setup("spawn preparation lock poisoned".into()))?;
        let preparation = match &mut *preparation {
            Some(preparation) => preparation,
            empty => empty.insert(
                self.backends
                    .project(
                        &self.configuration,
                        self.backend,
                        &self.store,
                        &self.cancellation,
                    )?
                    .preparation,
            ),
        };
        preparation.prepare_spawn(
            &self.configuration,
            &self.store,
            &self.cancellation,
            request,
            directory,
        )
    }
}
