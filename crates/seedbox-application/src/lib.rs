//! Host-independent configuration and orchestration for Seedbox.
//!
//! The CLI is one caller of this library. Image preparation and machine execution
//! implement separate traits; neither CLI parsing nor process-global environment
//! access belongs in the orchestration layer.

mod build_context;
pub mod config;
pub mod container_images;
pub mod definition;
pub use seedbox_artifacts::diff;
pub use seedbox_artifacts::engine;
pub use seedbox_artifacts::evidence;
pub mod execution;
pub mod fault_inputs;
pub mod guest;
pub mod image;
mod image_archive;
use seedbox_artifacts::limits;
mod driver_preparation;
pub mod oci;
pub mod orchestration;
pub mod preparation;
mod preparation_command;
pub use seedbox_artifacts::store;
pub mod templates;

pub use seedbox_artifacts::record;
use seedbox_artifacts::{invalid, Context};
pub use seedbox_artifacts::{Error, Result};
