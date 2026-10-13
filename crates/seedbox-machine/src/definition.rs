//! Common configured-machine inputs, independent of execution backend.
//!
//! TODO BEFORE MERGE: finalize this provisional definition API and the dynamic
//! launch representation with its preparation and Driver callers.
use std::{collections::BTreeMap, fmt};

/// Owned prepared application and resources for one named machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineDefinition {
    pub name: String,
    pub resources: Resources,
    pub application: PreparedApplication,
}
/// Requested machine resources; unsupported values are rejected, never ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resources {
    pub memory_bytes: u64,
}
/// A prepared filesystem and the resolved program to execute within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedApplication {
    pub filesystem: FilesystemArchive,
    pub launch: Launch,
}
/// Owned, validated flattened filesystem archive, preserving Linux metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesystemArchive {
    pub(crate) bytes: Vec<u8>,
}
impl FilesystemArchive {
    /// Validate and own a flattened tar filesystem without extracting it.
    pub fn new(bytes: Vec<u8>) -> Result<Self, DefinitionError> {
        crate::application::validate_filesystem(&bytes)?;
        Ok(Self { bytes })
    }
}
/// Resolved arguments, environment and identity for the prepared program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// Executable followed by arguments; no shell parses this vector.
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: String,
    /// User or UID, optionally followed by group or GID, within the filesystem.
    pub user: String,
}
/// Invalid prepared application or configured-machine input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionError(pub(crate) String);
impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DefinitionError {}
