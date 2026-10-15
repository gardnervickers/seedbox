//! Raw diagnostics resolved only from server-owned records and digest keys.
use super::*;

impl ExecutionService {
    fn diagnostic_record(
        &self,
        id: &SimulationId,
    ) -> Result<Option<seedbox_artifacts::record::RunRecord>, Error> {
        let resource = self.manager.describe(id)?;
        if resource.status.phase != Phase::Completed
            || resource.metadata.deletion_requested_at_unix_ms.is_some()
        {
            return Err(catalogue::Error::Invalid(
                "diagnostics require a completed simulation not being deleted".into(),
            )
            .into());
        }
        if self.manager.read(|catalogue| catalogue.is_imported(id))? {
            return Ok(self.imported_snapshot(id)?.record);
        }
        let file = match fs::File::open(self.root.join("inputs").join(format!("{id}.json"))) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let inputs: RetainedInputs =
            serde_json::from_reader(file).map_err(seedbox_application::Error::from)?;
        inputs
            .attempt
            .map(|attempt| seedbox_artifacts::record::inspect(&attempt).map_err(Error::from))
            .transpose()
    }

    pub fn diagnostics(&self, id: &SimulationId) -> Result<seedbox_api::DiagnosticFiles, Error> {
        let _guard = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let out_of_memory_kills = self.out_of_memory_kills(id)?;
        let Some(record) = self.diagnostic_record(id)? else {
            return Ok(seedbox_api::DiagnosticFiles {
                files: Vec::new(),
                omissions: BTreeMap::from([(
                    "record".into(),
                    "execution record unavailable".into(),
                )]),
                out_of_memory_kills,
            });
        };
        let mut files = Vec::new();
        for (name, digest) in record.logs {
            files.push(seedbox_api::DiagnosticFile {
                name,
                sha256: digest.hex().into(),
                size_bytes: self.store.size(&digest)?,
            });
        }
        Ok(seedbox_api::DiagnosticFiles {
            files,
            omissions: record.omitted_logs,
            out_of_memory_kills,
        })
    }

    /// Guest kernel out-of-memory kills the server observed, in simulated time
    /// order, with each machine's configured memory.
    fn out_of_memory_kills(
        &self,
        id: &SimulationId,
    ) -> Result<Vec<seedbox_api::MachineOutOfMemoryKill>, Error> {
        let configuration = self.manager.describe(id)?.spec.configuration;
        let mut kills: Vec<_> =
            self.manager
                .world(id)?
                .machines
                .into_iter()
                .flat_map(|machine| {
                    let memory_mib = configuration.services.get(&machine.name).map(|service| {
                        service.memory_mb.unwrap_or(configuration.machine.memory_mb)
                    });
                    machine.out_of_memory_kills.into_iter().map(move |kill| {
                        seedbox_api::MachineOutOfMemoryKill {
                            machine: machine.name.clone(),
                            memory_mib,
                            kill,
                        }
                    })
                })
                .collect();
        kills.sort_by(|a, b| {
            (a.kill.simulated_time_ns, &a.machine).cmp(&(b.kill.simulated_time_ns, &b.machine))
        });
        Ok(kills)
    }

    /// Copy and verify under the evidence lock; HTTP owns the completed spool.
    pub fn diagnostic_file(
        &self,
        id: &SimulationId,
        name: &str,
        max_bytes: u64,
        output: &mut impl Write,
    ) -> Result<seedbox_api::DiagnosticFile, Error> {
        let _guard = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let record = self
            .diagnostic_record(id)?
            .ok_or_else(|| catalogue::Error::Invalid("execution record unavailable".into()))?;
        let digest = record.logs.get(name).ok_or_else(|| {
            catalogue::Error::Invalid(format!(
                "diagnostic {name:?}: unavailable; choose an exact name from diagnostics"
            ))
        })?;
        let file = seedbox_api::DiagnosticFile {
            name: name.into(),
            sha256: digest.hex().into(),
            size_bytes: self.store.size(digest)?,
        };
        if file.size_bytes > max_bytes {
            return Err(
                catalogue::Error::Invalid("diagnostic exceeds wire byte limit".into()).into(),
            );
        }
        self.store.copy_to(digest, output)?;
        Ok(file)
    }
}
