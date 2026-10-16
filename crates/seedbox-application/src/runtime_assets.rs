//! Offline-built inputs for the service's concrete Machine factories.
//!
//! A package owns guest boot inputs and the complete host QEMU loader/library
//! closure. Retain [`Runtime::archive`] in the content store with the execution
//! inventory, not just the selected QEMU launcher. Runtime loading never builds,
//! searches the environment, or executes a guest or host tool.
use crate::{
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_spec::definition::Architecture;
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

include!(concat!(env!("OUT_DIR"), "/driver-runtime.rs"));
mod archive;
const COMPRESSED_LIMIT: u64 = 1 << 30;
const UNPACKED_LIMIT: u64 = 4 << 30;
const MEMBER_LIMIT: usize = 16_384;

/// Factory selection, independent of worker transport or execution policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// Native PV remains the default backend.
    #[default]
    Pv,
    /// Software emulation of x86-64 Linux, on either supported host ISA.
    Tcg,
}

/// A privately extracted, immutable runtime package. Clones share its lifetime.
#[derive(Clone, Debug)]
pub struct Runtime {
    directory: Arc<tempfile::TempDir>,
}

/// Explicit factory inputs. This value keeps the entire extracted package alive.
/// Fields are paths, not authority to launch a client-side execution.
#[derive(Clone, Debug)]
pub struct Inputs {
    /// Directory consumed by the concrete Factory, including guest `mke2fs`.
    pub guest_assets: PathBuf,
    /// Host-native QEMU launcher and its adjacent retained closure, for TCG only.
    pub qemu: Option<PathBuf>,
    runtime: Runtime,
}
impl Inputs {
    /// Complete package for retention/export. Its lifetime is this value's lifetime.
    pub fn archive(&self) -> PathBuf {
        self.runtime.archive()
    }
}

impl Runtime {
    /// Read an explicitly supplied gzip tar package. Links, special files,
    /// duplicate/unsafe names, oversized members and unsupported host packages
    /// fail before this method returns. All extraction is beneath the explicit
    /// existing `scratch` directory; no temporary-directory environment is read.
    /// No partial extraction is published. Clones keep their private child alive.
    pub fn from_archive(path: &Path, scratch: &Path) -> Result<Self> {
        let file = fs::File::open(path).map_err(|error| Error::Setup(format!("driver runtime {}: {error}; run cargo xtask build-assets or supply a complete runtime archive", path.display())))?;
        if file.metadata()?.len() > COMPRESSED_LIMIT {
            return Err(invalid("compressed driver runtime exceeds 1 GiB"));
        }
        Self::load(file, scratch)
    }
    /// Read already-owned embedded or retained package bytes, with the same
    /// checks as [`Self::from_archive`]. No ambient asset lookup is performed.
    pub fn from_bytes(bytes: &[u8], scratch: &Path) -> Result<Self> {
        if bytes.len() as u64 > COMPRESSED_LIMIT {
            return Err(invalid("compressed driver runtime exceeds 1 GiB"));
        }
        Self::load(bytes, scratch)
    }
    /// Restore from content-owned bytes, independent of an original run directory.
    pub fn from_store(
        store: &ContentStore,
        digest: &ContentDigest,
        scratch: &Path,
    ) -> Result<Self> {
        if store.size(digest)? > COMPRESSED_LIMIT {
            return Err(invalid("compressed driver runtime exceeds 1 GiB"));
        }
        let temporary = tempfile::NamedTempFile::new_in(scratch).map_err(|error| {
            Error::Setup(format!(
                "driver runtime scratch {}: {error}; supply an existing writable session directory",
                scratch.display()
            ))
        })?;
        store.copy_to(digest, &mut temporary.as_file())?;
        Self::from_archive(temporary.path(), scratch)
    }
    fn load(mut reader: impl Read, scratch: &Path) -> Result<Self> {
        let directory = Arc::new(tempfile::Builder::new().prefix("driver-runtime-").tempdir_in(scratch)
            .map_err(|error| Error::Setup(format!("driver runtime scratch {}: {error}; supply an existing writable session directory", scratch.display())))?);
        let archive_path = directory.path().join("runtime.tar.gz");
        let mut archive_file = fs::File::create(&archive_path)?;
        let length = std::io::copy(
            &mut reader.by_ref().take(COMPRESSED_LIMIT + 1),
            &mut archive_file,
        )?;
        if length > COMPRESSED_LIMIT {
            return Err(invalid("compressed driver runtime exceeds 1 GiB"));
        }
        drop(archive_file);
        let root = directory.path().join("assets");
        fs::create_dir(&root)?;
        let mut archive = tar::Archive::new(flate2::read::MultiGzDecoder::new(fs::File::open(
            &archive_path,
        )?));
        archive::extract(&mut archive, &root)?;
        // Tar stops at its zero end markers. Drain only bounded zero padding
        // through gzip so truncated trailers, checksum failures and hidden
        // suffix payloads cannot be mistaken for a complete package.
        let mut decoder = archive.into_inner();
        let mut padding = [0u8; 8192];
        let mut padding_size = 0usize;
        loop {
            let read = decoder.read(&mut padding)?;
            if read == 0 {
                break;
            }
            padding_size += read;
            if padding_size > (1 << 20) || padding[..read].iter().any(|byte| *byte != 0) {
                return Err(invalid("unexpected data after driver runtime tar end"));
            }
        }
        let mut system = String::new();
        fs::File::open(root.join("host-system")).and_then(|file| {
            file.take(archive::HOST_SYSTEM_LIMIT + 1).read_to_string(&mut system)
        }).map_err(|error| Error::Setup(format!("driver runtime missing or invalid host-system: {error}; run cargo xtask build-assets")))?;
        if system.len() as u64 > archive::HOST_SYSTEM_LIMIT {
            return Err(invalid("driver runtime host-system exceeds 64 bytes"));
        }
        if system != host_system() {
            return Err(Error::Setup(format!("driver runtime host {system:?} does not match {}; run cargo xtask build-assets on the target host", host_system())));
        }
        Ok(Self { directory })
    }
    /// The original complete package. Keep its bytes in the execution's content
    /// inventory so replay/export survives source execution or Nix-root deletion.
    pub fn archive(&self) -> PathBuf {
        self.directory.path().join("runtime.tar.gz")
    }
    /// Retain all host libraries and guest inputs as one verified content asset.
    pub fn retain(&self, store: &ContentStore) -> Result<ContentDigest> {
        store.put_file(&self.archive())
    }
    /// Select inputs by requested backend and guest ISA, never blindly by host ISA.
    /// PV requires the native guest ISA; TCG accepts only x86-64 on every host.
    /// Guest files are read as bytes; their host execute bits are irrelevant.
    /// Guest ELF ISA and real host executable modes are checked here; concrete
    /// factories validate complete formats and give guest boot files their modes.
    // TODO: Re-evaluate: merge this small selection enum with the service's
    // backend type if that type becomes transport-independent and public.
    pub fn select(&self, backend: Backend, architecture: Architecture) -> Result<Inputs> {
        let arch = match architecture {
            Architecture::X86_64 => "x86_64",
            Architecture::Aarch64 => "aarch64",
        };
        if (backend == Backend::Tcg && architecture != Architecture::X86_64)
            || (backend == Backend::Pv && arch != std::env::consts::ARCH)
        {
            return Err(Error::Setup(format!("{backend:?} does not support {arch} guests on {}; PV requires the native ISA and TCG requires x86_64", host_system())));
        }
        let root = self.directory.path().join("assets");
        let guest = root
            .join(if backend == Backend::Pv { "pv" } else { "tcg" })
            .join(arch);
        for name in [
            "kernel",
            "init",
            "command-line",
            "mke2fs",
            if backend == Backend::Pv {
                "progress.tar"
            } else {
                "firmware"
            },
        ] {
            regular(&guest.join(name), name == "command-line")?;
        }
        let expected_machine = if architecture == Architecture::X86_64 {
            62
        } else {
            183
        };
        for name in ["init", "mke2fs"] {
            guest_elf(&guest.join(name), expected_machine)?;
        }
        let qemu = if backend == Backend::Tcg {
            let host = root.join("host").join(host_system());
            let launcher = host.join("bin/qemu-system-x86_64");
            executable(&launcher)?;
            let real = host.join("libexec/qemu-system-x86_64");
            executable(&real)?;
            host_executable(&real)?;
            let libraries = fs::read_dir(host.join("lib")).map_err(|error| Error::Setup(format!("driver runtime QEMU library closure missing: {error}; run cargo xtask build-assets")))?;
            if libraries.count() == 0 {
                return Err(invalid("driver runtime QEMU library closure is empty"));
            }
            regular(&host.join("share/qemu/qboot.rom"), false)?;
            Some(launcher)
        } else {
            None
        };
        Ok(Inputs {
            guest_assets: guest,
            qemu,
            runtime: self.clone(),
        })
    }
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Integrity(message.into())
}
fn host_system() -> String {
    format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    )
}
fn allowed(name: &str) -> bool {
    if name
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == ".." || part.contains('\\'))
    {
        return false;
    }
    if name == "host-system" {
        return true;
    }
    let parts: Vec<_> = name.split('/').collect();
    match parts.as_slice() {
        ["pv", "aarch64" | "x86_64", "kernel" | "init" | "command-line" | "progress.tar" | "mke2fs"] => {
            true
        }
        ["tcg", "x86_64", "kernel" | "init" | "command-line" | "firmware" | "mke2fs"] => true,
        ["host", system, "bin" | "libexec", "qemu-system-x86_64"] => *system == host_system(),
        ["host", system, "lib", _] => *system == host_system(),
        ["host", system, "share", "qemu", "qboot.rom"] => *system == host_system(),
        _ => false,
    }
}
fn regular(path: &Path, empty: bool) -> Result<()> {
    let metadata = fs::metadata(path).map_err(|error| {
        Error::Setup(format!(
            "driver runtime {}: {error}; run cargo xtask build-assets",
            path.display()
        ))
    })?;
    if !metadata.is_file() || (!empty && metadata.len() == 0) {
        return Err(invalid(format!(
            "missing or empty runtime input {}",
            path.display()
        )));
    }
    Ok(())
}
fn executable(path: &Path) -> Result<()> {
    regular(path, false)?;
    if fs::metadata(path)?.permissions().mode() & 0o111 == 0 {
        return Err(invalid(format!(
            "runtime input is not executable: {}",
            path.display()
        )));
    }
    Ok(())
}
fn host_executable(path: &Path) -> Result<()> {
    if cfg!(target_os = "linux") {
        guest_elf(
            path,
            if cfg!(target_arch = "x86_64") {
                62
            } else {
                183
            },
        )
    } else {
        let mut header = [0; 8];
        fs::File::open(path)?.read_exact(&mut header)?;
        let expected_cpu = if cfg!(target_arch = "aarch64") {
            0x0100_000c_u32
        } else {
            0x0100_0007_u32
        };
        if u32::from_le_bytes(
            header[..4]
                .try_into()
                .map_err(|_| invalid("Mach-O magic"))?,
        ) != 0xfeed_facf
            || u32::from_le_bytes(header[4..].try_into().map_err(|_| invalid("Mach-O CPU"))?)
                != expected_cpu
        {
            return Err(invalid(
                "QEMU host executable has wrong Mach-O architecture",
            ));
        }
        Ok(())
    }
}
fn guest_elf(path: &Path, machine: u16) -> Result<()> {
    let mut header = [0; 20];
    fs::File::open(path)?
        .read_exact(&mut header)
        .map_err(|error| invalid(format!("guest ELF {}: {error}", path.display())))?;
    if &header[..6] != b"\x7fELF\x02\x01" || u16::from_le_bytes([header[18], header[19]]) != machine
    {
        return Err(invalid(format!(
            "guest ELF {} has wrong architecture",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
