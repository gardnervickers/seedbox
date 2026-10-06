//! Host-independent configuration and orchestration for Seedbox.
//!
//! The CLI is one caller of this library. Image preparation and machine execution
//! implement separate traits; neither CLI parsing nor process-global environment
//! access belongs in the orchestration layer.

#![doc = include_str!("../README.md")]

pub mod config;
pub mod execution;
pub mod image;
pub mod orchestration;
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
    #[error("execution: {0}")]
    Execution(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid<T>(path: &str, message: impl Into<String>) -> Result<T> {
    Err(Error::Validation {
        path: path.into(),
        message: message.into(),
    })
}
