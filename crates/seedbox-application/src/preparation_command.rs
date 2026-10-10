//! Cancellation and retained setup evidence for native image preparation.
use crate::{execution::CancellationToken, store::ContentDigest, Error, Result};
use std::{collections::BTreeMap, fs, io::Write, path::PathBuf};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationStage {
    Image,
    Guest,
    Ready,
}

type ProgressObserver = dyn Fn(&str, PreparationStage) + Send + Sync;

pub struct PreparationContext {
    pub cancellation: CancellationToken,
    pub directory: PathBuf,
    progress: Option<std::sync::Arc<ProgressObserver>>,
}
impl std::fmt::Debug for PreparationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparationContext")
            .field("directory", &self.directory)
            .field("cancellation", &self.cancellation)
            .finish_non_exhaustive()
    }
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
            progress: None,
        })
    }
    /// Optional observation sink. It must return promptly and never owns work.
    pub fn with_progress(
        mut self,
        progress: impl Fn(&str, PreparationStage) + Send + Sync + 'static,
    ) -> Self {
        self.progress = Some(std::sync::Arc::new(progress));
        self
    }
    pub(crate) fn progress(&self, machine: &str, stage: PreparationStage) {
        if let Some(progress) = &self.progress {
            progress(machine, stage);
        }
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
