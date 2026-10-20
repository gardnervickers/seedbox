//! Common configured-machine inputs, independent of execution backend.
//!
//! One definition owns a guest's resources and prepared launch.
//! Splitting instance overrides from the definition would hide part of what runs.
//! Backend boot inputs belong to concrete factories; caller-side templating needs no API.
use std::{collections::BTreeMap, fmt, sync::Arc};

mod network;
pub use network::{MacAddress, NetworkInterface};
mod policy;
pub use policy::BackendPolicy;

/// Owned prepared application and resources for one named machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineDefinition {
    pub name: String,
    pub resources: Resources,
    pub application: PreparedApplication,
    /// Initial durable contents of the one attached block device, exposed to
    /// the guest as `/dev/vda`; `None` attaches no block device. Nothing
    /// mounts it: the application owns its use.
    pub disk: Option<DiskImage>,
    /// The machine's Ethernet interface; `None` attaches no NIC.
    pub network: Option<NetworkInterface>,
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
/// Explicit validation budgets for an imported flattened archive. Logical
/// entry/path/file-content accounting is not a process RSS guarantee.
#[derive(Clone, Copy, Debug)]
pub struct ArchiveValidationLimits {
    /// Raw extension/file records plus implicit directory nodes.
    pub entries: u64,
    /// Aggregate retained path/link bytes; bounds extension paths before allocation.
    pub path_bytes: u64,
    /// Aggregate bytes read from regular file contents during validation.
    pub expanded_bytes: u64,
    /// Logical node/path index and GNU/PAX/prefix-buffer storage. Not process RSS.
    pub metadata_bytes: u64,
}
impl ArchiveValidationLimits {
    pub(crate) const UNLIMITED: Self = Self {
        entries: u64::MAX,
        path_bytes: u64::MAX,
        expanded_bytes: u64::MAX,
        metadata_bytes: u64::MAX,
    };
}
impl FilesystemArchive {
    /// Borrow exact immutable validated tar bytes; never extracts guest paths.
    // Persistence streams these original bytes, not a repacked boot archive.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Validate and own imported bytes under explicit logical budgets. File
    /// contents are streamed during validation, not duplicated into node data.
    pub fn new_bounded(
        bytes: Vec<u8>,
        limits: ArchiveValidationLimits,
    ) -> Result<Self, DefinitionError> {
        crate::application::validate_filesystem_bounded(&bytes, limits)?;
        Ok(Self { bytes })
    }
    /// Validate and own a flattened tar filesystem without extracting it.
    pub fn new(bytes: Vec<u8>) -> Result<Self, DefinitionError> {
        crate::application::validate_filesystem(&bytes)?;
        Ok(Self { bytes })
    }
}
/// Validated initial durable contents of one block device, in bytes.
///
/// A definition fixes only the initial durable contents when a machine is
/// first constructed; later boots retain that machine's durable writes. Guest
/// writes, flushes and boot-end loss change the machine's disk, never its
/// definition. Capacity is nonzero and a whole number of 512-byte
/// sectors. Image bytes are immutable and may be shared between clones.
/// Equality compares capacity and contents, however they were constructed.
// One owned image or zero fill covers caller-supplied
// filesystems without host paths or backend-specific formats; sparse or
// layered images can be added as constructors without changing this type.
#[derive(Clone)]
pub struct DiskImage {
    contents: DiskContents,
}
#[derive(Clone)]
enum DiskContents {
    /// Zero-filled capacity in bytes, never materialized.
    Zero(u64),
    Image(Arc<[u8]>),
}
impl DiskImage {
    /// A zero-filled disk of `capacity_bytes`, without allocating its contents.
    pub fn zeroed(capacity_bytes: u64) -> Result<Self, DefinitionError> {
        validate_capacity(capacity_bytes)?;
        Ok(Self {
            contents: DiskContents::Zero(capacity_bytes),
        })
    }
    /// A disk whose initial contents and capacity are exactly `bytes`.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, DefinitionError> {
        validate_capacity(bytes.len() as u64)?;
        Ok(Self {
            contents: DiskContents::Image(bytes.into()),
        })
    }
    /// Capacity in bytes, a nonzero multiple of 512.
    pub fn capacity_bytes(&self) -> u64 {
        match &self.contents {
            DiskContents::Zero(capacity) => *capacity,
            DiskContents::Image(image) => image.len() as u64,
        }
    }
    /// Capacity in 512-byte sectors, without allocation or mutation.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn sectors(&self) -> u64 {
        self.capacity_bytes() / SECTOR_BYTES
    }
    /// Borrow the immutable initial image, or return None for zero fill.
    /// Cloning the Arc shares storage; this view includes no later guest writes.
    // Backends prepare owned backing inputs from this borrow; a copy would
    // materialize zero fill or duplicate the image.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn image(&self) -> Option<&Arc<[u8]>> {
        match &self.contents {
            DiskContents::Zero(_) => None,
            DiskContents::Image(image) => Some(image),
        }
    }
}
const SECTOR_BYTES: u64 = 512;
fn validate_capacity(capacity_bytes: u64) -> Result<(), DefinitionError> {
    if capacity_bytes == 0 || !capacity_bytes.is_multiple_of(SECTOR_BYTES) {
        return Err(DefinitionError(format!(
            "disk capacity {capacity_bytes} bytes must be a nonzero multiple of {SECTOR_BYTES}"
        )));
    }
    Ok(())
}
impl PartialEq for DiskImage {
    fn eq(&self, other: &Self) -> bool {
        use DiskContents::{Image, Zero};
        match (&self.contents, &other.contents) {
            (Zero(a), Zero(b)) => a == b,
            (Image(a), Image(b)) => Arc::ptr_eq(a, b) || a == b,
            (Zero(capacity), Image(image)) | (Image(image), Zero(capacity)) => {
                image.len() as u64 == *capacity && image.iter().all(|&byte| byte == 0)
            }
        }
    }
}
impl Eq for DiskImage {}
impl fmt::Debug for DiskImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.contents {
            DiskContents::Zero(_) => "zeroed",
            DiskContents::Image(_) => "image",
        };
        f.debug_struct("DiskImage")
            .field("contents", &kind)
            .field("capacity_bytes", &self.capacity_bytes())
            .finish()
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

#[cfg(test)]
mod tests;
