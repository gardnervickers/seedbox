//! Guest architecture and boot-sizing floors shared by preparation.

use crate::{Error, Result};
use std::{fs, io::Read, path::Path};

/// Guest ISA comes from the retained execution profile, never host tools or an
/// inherited image-platform setting. Descriptorless non-native helpers use amd64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    Amd64,
    Arm64,
}
impl Architecture {
    pub fn from_profile(profile: &crate::execution::ExecutionProfile) -> Result<Self> {
        let engine = profile.semantic_parameters.engine.as_ref();
        match engine.map(|engine| engine.guest_arch.as_str()) {
            None => Ok(Self::Amd64),
            Some("x86_64") => Ok(Self::Amd64),
            Some("aarch64") => Ok(Self::Arm64),
            value => Err(Error::Integrity(format!(
                "unsupported guest architecture {value:?}"
            ))),
        }
    }
    pub fn oci_name(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
    pub fn platform(self) -> &'static str {
        match self {
            Self::Amd64 => "linux/amd64",
            Self::Arm64 => "linux/arm64",
        }
    }
    fn elf_machine(self) -> [u8; 2] {
        match self {
            Self::Amd64 => [62, 0],
            Self::Arm64 => [183, 0],
        }
    }
    pub(crate) fn validate_elf(self, path: &Path) -> Result<()> {
        let mut header = [0; 20];
        fs::File::open(path)
            .and_then(|mut file| file.read_exact(&mut header))
            .map_err(|error| {
                Error::Setup(format!(
                    "guest helper {}: {error}; provide a static {} ELF",
                    path.display(),
                    self.oci_name()
                ))
            })?;
        if &header[..6] != b"\x7fELF\x02\x01" || header[18..20] != self.elf_machine() {
            return Err(Error::Integrity(format!(
                "expected an {} ELF guest helper: {}",
                self.oci_name(),
                path.display()
            )));
        }
        Ok(())
    }
}

/// Conservative boot floor for the controlled kernel: the input archive is
/// resident while root tmpfs is populated, and tmpfs's default capacity is half
/// the available RAM. Reserve additional space for the kernel and bootstrap.
/// Application heap/storage requirements can require more than this floor.
pub fn minimum_memory_mb(archive_bytes: u64) -> Result<u32> {
    let bytes = archive_bytes
        .checked_mul(3)
        .and_then(|bytes| bytes.checked_add(128 * 1024 * 1024))
        .ok_or_else(|| Error::Execution("guest memory estimate overflow".into()))?;
    u32::try_from(bytes.div_ceil(1024 * 1024))
        .map_err(|_| Error::Execution("guest memory estimate exceeds supported range".into()))
}

/// Linux rejects a CLOCK_REALTIME earlier than the guest's uptime, so an epoch
/// below this would stop every guest at the launch barrier.
pub const MIN_WALL_CLOCK_EPOCH: u32 = 86400;
