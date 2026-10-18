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
#[cfg(unix)]
pub mod driver_world;
pub use seedbox_artifacts::diff;
pub use seedbox_artifacts::engine;
pub use seedbox_artifacts::evidence;
pub use seedbox_artifacts::exec_files;
pub mod execution;
pub mod exploration;
pub mod fault_inputs;
pub mod guest;
#[cfg(any(target_os = "linux", test))]
mod host;
pub mod host_control;
pub mod image;
mod image_archive;
use seedbox_artifacts::limits;
pub use seedbox_artifacts::logs;
pub mod oci;
pub mod orchestration;
pub mod preparation;
mod preparation_command;
pub mod runtime_assets;
pub use seedbox_artifacts::store;
pub mod templates;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod worker_process;

pub use seedbox_artifacts::record;
use seedbox_artifacts::{invalid, Context};
pub use seedbox_artifacts::{Error, Result};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod process_lifetime;
