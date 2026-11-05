//! The round trip that prepares a spawned machine while its run waits. The
//! worker writes `<name>.request.json` in the attempt's spawn directory and the
//! parent answers with `<name>.response.json` beside it. Both are written whole
//! and then renamed, so a reader never sees half a file.
use super::*;
use seedbox_engine_protocol::driver_world::{Environment, SpawnRequest, SpawnResponse, Spawned};
use std::{fs, io::Write};

/// The attempt output subdirectory both sides of the round trip use.
pub(crate) const DIRECTORY: &str = "spawn";
const REQUEST: &str = ".request.json";
const RESPONSE: &str = ".response.json";

/// Write a file whole under a temporary name, then rename it into place.
pub(crate) fn publish(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(directory.join(name)).map_err(|e| e.error)?;
    Ok(())
}

/// Ask the parent for the machine `request` names, once per spawn.
pub(super) fn ask(output: &Path, request: &driver::SpawnRequest) -> Result<()> {
    let directory = output.join(DIRECTORY);
    fs::create_dir_all(&directory)?;
    let file = format!("{}{REQUEST}", request.name);
    if directory.join(&file).exists() {
        return Ok(());
    }
    // Spawned machines take the addresses after every initial machine.
    let number = u8::try_from(request.machine.index() + 1)
        .ok()
        .filter(|number| *number < u8::MAX)
        .ok_or_else(|| fail("a run supports at most 254 machines"))?;
    let ask = SpawnRequest {
        name: request.name.clone(),
        spec: request.spec.clone(),
        number,
    };
    publish(&directory, &file, &serde_json::to_vec(&ask)?)
}

/// The parent's answer for `name`, once it has written one.
pub(in crate::driver_world) fn answer(output: &Path, name: &str) -> Result<Option<SpawnResponse>> {
    let path = output.join(DIRECTORY).join(format!("{name}{RESPONSE}"));
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The names of requests in `directory` that have no response yet.
pub(crate) fn unanswered(directory: &Path) -> Result<Vec<(String, SpawnRequest)>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    let mut requests = Vec::new();
    for entry in entries {
        let file = entry?.file_name();
        let Some(name) = file.to_str().and_then(|file| file.strip_suffix(REQUEST)) else {
            continue;
        };
        if directory.join(format!("{name}{RESPONSE}")).exists() {
            continue;
        }
        let request: SpawnRequest = serde_json::from_slice(&fs::read(directory.join(&file))?)?;
        requests.push((name.to_owned(), request));
    }
    requests.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(requests)
}

/// Answer the request for `name`.
pub(crate) fn respond(directory: &Path, name: &str, response: &SpawnResponse) -> Result<()> {
    publish(
        directory,
        &format!("{name}{RESPONSE}"),
        &serde_json::to_vec(response)?,
    )
}

/// Every machine the parent prepared in this attempt, by name.
pub(crate) fn prepared(directory: &Path) -> Result<Vec<(Spawned, Environment)>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let file = entry?.file_name();
        if let Some(name) = file.to_str().and_then(|file| file.strip_suffix(RESPONSE)) {
            names.push(name.to_owned());
        }
    }
    names.sort();
    let mut prepared = Vec::new();
    for name in names {
        let path = directory.join(format!("{name}{RESPONSE}"));
        if let SpawnResponse::Prepared {
            spawned,
            environment,
        } = serde_json::from_slice(&fs::read(path)?)?
        {
            prepared.push((*spawned, *environment));
        }
    }
    Ok(prepared)
}

impl<M> Worker<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    /// Give the waiting spawn the machine the parent prepared for it.
    pub(super) fn provide_spawn(
        &mut self,
        request: &driver::SpawnRequest,
        spawned: Spawned,
        environment: Environment,
    ) -> Result<()> {
        if spawned.spec != request.spec || spawned.machine.name != request.name {
            return Err(fail(format!(
                "the machine prepared for spawn {} answers a different request",
                request.name
            )));
        }
        for artifact in environment.configs.values() {
            super::super::checkpoint::verified(artifact)?;
        }
        let definition = super::super::build::definition(&spawned.machine)?;
        let environments = self
            .environments
            .as_ref()
            .ok_or_else(|| fail("this worker cannot construct spawned machines"))?;
        environments
            .lock()
            .map_err(|_| fail("factory environments poisoned"))?
            .insert(request.name.clone(), environment.clone());
        match &mut self.inventory {
            Inventory::Pv { environments, .. } | Inventory::Tcg { environments, .. } => {
                environments.insert(request.name.clone(), environment);
            }
        }
        self.driver
            .with_barrier(|barrier| {
                barrier.provide(driver::SpawnDefinition {
                    spec: spawned.spec,
                    definition,
                    allow_crashes: spawned.machine.allow_crashes,
                })
            })
            .map_err(|error| fail(format!("spawn {}: {error}", request.name)))
    }
}
