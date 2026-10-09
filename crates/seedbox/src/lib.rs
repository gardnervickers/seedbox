//! Host-independent configuration and orchestration for Seedbox.
//!
//! The CLI is one caller of this library. Image preparation and machine execution
//! implement separate traits; neither CLI parsing nor process-global environment
//! access belongs in the orchestration layer.

#![doc = include_str!("../README.md")]

pub mod admission;
mod build_context;
pub mod bundle;
pub mod config;
pub mod container_images;
pub mod diff;
pub mod engine;
pub mod execution;
pub mod exploration;
pub mod guest;
pub mod guest_assets;
#[cfg(any(target_os = "linux", test))]
mod host;
pub mod host_control;
pub mod image;
mod image_archive;
pub mod logs;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod mac;
pub mod oci;
pub mod orchestration;
pub mod preparation;
mod preparation_command;
mod retained;
pub mod store;
pub mod templates;
pub mod vmm;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod worker_process;

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

pub(crate) fn invalid<T>(path: &str, message: impl Into<String>) -> Result<T> {
    Err(Error::Validation {
        path: path.into(),
        message: message.into(),
    })
}
