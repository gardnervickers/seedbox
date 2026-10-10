//! Offline retained-run storage, inspection, comparison and transfer.
//! No image engine, asynchronous runtime or hypervisor dependency.
pub mod bundle;
pub mod comparison;
pub mod config;
pub mod diff;
pub mod engine;
pub mod evidence;
pub mod execution;
pub mod image;
pub mod limits;
pub mod logs;
pub mod record;
mod retained;
pub mod store;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {message}")]
    Validation { path: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("input integrity: {0}")]
    Integrity(String),
    #[error("preparation cancelled")]
    PreparationCancelled,
    #[error("preparation cleanup-unverified: {0}")]
    PreparationCleanupUnverified(String),
    #[error("execution: {0}")]
    Execution(String),
    /// A missing or unusable input or host tool the caller can fix; the
    /// message says how.
    #[error("{0}")]
    Setup(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Prefix an error with what was being done, in one of `Error`'s categories:
/// `.context(Error::Setup, path.display())` reads as `"{path}: {error}"`.
pub trait Context<T> {
    fn context(self, category: fn(String) -> Error, context: impl std::fmt::Display) -> Result<T>;
}
impl<T, E: std::fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, category: fn(String) -> Error, context: impl std::fmt::Display) -> Result<T> {
        self.map_err(|error| category(format!("{context}: {error}")))
    }
}

pub fn invalid<T>(path: &str, message: impl Into<String>) -> Result<T> {
    Err(Error::Validation {
        path: path.into(),
        message: message.into(),
    })
}
