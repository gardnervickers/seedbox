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
        let resource = self.manager.describe(id)?;
        let out_of_memory_kills = self
            .manager
            .world(id)?
            .machines
            .into_iter()
            .flat_map(|machine| {
                let memory_mib =
                    resource
                        .spec
                        .configuration
                        .services
                        .get(&machine.name)
                        .map(|service| {
                            service
                                .memory_mb
                                .unwrap_or(resource.spec.configuration.machine.memory_mb)
                        });
                machine.out_of_memory_kills.into_iter().map(move |report| {
                    seedbox_api::OutOfMemoryDiagnostic {
                        machine: machine.name.clone(),
                        report,
                        memory_mib,
                    }
                })
            })
            .collect();
        let Some(record) = self.diagnostic_record(id)? else {
            return Ok(seedbox_api::DiagnosticFiles {
                out_of_memory_kills,
                files: Vec::new(),
                omissions: BTreeMap::from([(
                    "record".into(),
                    "execution record unavailable".into(),
                )]),
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
            out_of_memory_kills,
            files,
            omissions: record.omitted_logs,
        })
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
