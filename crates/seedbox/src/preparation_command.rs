//! Cancellation and retained setup evidence for native image preparation.
use crate::{execution::CancellationToken, store::ContentDigest, Error, Result};
use std::{collections::BTreeMap, fs, io::Write, path::PathBuf};
#[derive(Debug)]
pub struct PreparationContext {
    pub cancellation: CancellationToken,
    pub directory: PathBuf,
}
impl PreparationContext {
    pub fn new(directory: impl Into<PathBuf>, cancellation: CancellationToken) -> Result<Self> {
        let directory = directory.into();
        fs::create_dir(&directory).map_err(|error| {
            Error::Setup(format!(
                "preparation evidence {}: {error}",
                directory.display()
            ))
        })?;
        Ok(Self {
            cancellation,
            directory,
        })
    }
    pub fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            Err(Error::PreparationCancelled)
        } else {
            Ok(())
        }
    }
    pub(crate) fn setup_inputs(
        &self,
        auxiliary: &BTreeMap<String, ContentDigest>,
        engine: &ContentDigest,
        kernel: &ContentDigest,
    ) -> Result<()> {
        self.check()?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.write_all(&serde_json::to_vec_pretty(
            &serde_json::json!({"auxiliary": auxiliary, "engine": engine, "kernel": kernel}),
        )?)?;
        file.persist(self.directory.join("setup.json"))
            .map_err(|e| e.error)?;
        Ok(())
    }
}

// Fork/process tests in other modules share one process-creation lock.
#[cfg(test)]
pub(crate) static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
