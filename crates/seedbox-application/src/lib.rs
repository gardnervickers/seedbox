//! Host-independent configuration and orchestration for Seedbox.
//!
//! The CLI is one caller of this library. Image preparation and machine execution
//! implement separate traits; neither CLI parsing nor process-global environment
//! access belongs in the orchestration layer.

pub mod admission;
mod build_context;
pub mod config;
pub mod container_images;
pub mod definition;
pub use seedbox_artifacts::diff;
pub use seedbox_artifacts::engine;
pub mod engine_adapter;
pub mod execution;
pub mod exploration;
pub mod guest;
pub mod guest_assets;
#[cfg(any(target_os = "linux", test))]
mod host;
pub mod host_control;
pub mod image;
mod image_archive;
use seedbox_artifacts::limits;
pub use seedbox_artifacts::logs;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod mac;
pub mod oci;
pub mod orchestration;
pub mod preparation;
mod preparation_command;
pub use seedbox_artifacts::store;
pub mod templates;
pub mod vmm;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod worker_process;

pub use seedbox_artifacts::record;
use seedbox_artifacts::{invalid, Context};
pub use seedbox_artifacts::{Error, Result};

#[cfg(test)]
mod test_process;
