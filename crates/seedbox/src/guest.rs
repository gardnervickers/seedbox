//! Deterministic initramfs assembly around an imported image filesystem.
//!
//! Image file metadata and hard links are preserved; only Seedbox-owned bootstrap
//! files have normalized timestamps. The image runs in a chroot inside the guest.

use crate::{image::Launch, Error, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

/// Guest ISA comes from the retained execution profile, never host tools or an
/// inherited image-platform setting. Historical profiles retain amd64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    Amd64,
    Arm64,
}
impl Architecture {
    pub fn from_profile(profile: &crate::execution::ExecutionProfile) -> Result<Self> {
        match profile
            .semantic_parameters
            .get("guest_arch")
            .map(String::as_str)
        {
            None if profile.backend != "seedbox-hvf" => Ok(Self::Amd64),
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

/// This binary recipe is consumed by the small static Linux guest init. Length
/// prefixes avoid shell evaluation of command, environment or config contents.
pub fn launch_recipe(
    launch: &Launch,
    ip: &str,
    hosts: &str,
    hostname: &str,
    configs: &BTreeMap<String, Vec<u8>>,
    wall_clock_epoch: u32,
) -> Result<Vec<u8>> {
    let mut bytes = b"SBXLAUNCH4".to_vec();
    for value in [ip, hosts, hostname, &launch.user, &launch.working_dir] {
        field(&mut bytes, value.as_bytes())?;
    }
    // A guest-wide default per service VM, retained with the launch inputs.
    bytes.extend_from_slice(&262144_u32.to_le_bytes()); // vm.max_map_count
    bytes.extend_from_slice(&wall_clock_epoch.to_le_bytes());
    list(&mut bytes, &launch.argv)?;
    list(
        &mut bytes,
        &launch
            .environment
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>(),
    )?;
    list(&mut bytes, &launch.volumes)?;
    // Named volumes as name, target pairs, for the supervisor to mount from
    // the retained disk before it enters the image root.
    let mut volumes = Vec::new();
    for mount in &launch.disk_volumes {
        volumes.push(mount.source.clone());
        volumes.push(mount.target.clone());
    }
    list(&mut bytes, &volumes)?;
    if configs.len() > 65536 {
        return Err(Error::Execution("too many config mounts".into()));
    }
    bytes.extend_from_slice(&(configs.len() as u32).to_le_bytes());
    for (target, content) in configs {
        field(&mut bytes, target.as_bytes())?;
        field(&mut bytes, content)?;
    }
    Ok(bytes)
}

fn field(output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(Error::Execution("launch field exceeds 16 MiB".into()));
    }
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}
fn list(output: &mut Vec<u8>, items: &[String]) -> Result<()> {
    if items.len() > 65536 {
        return Err(Error::Execution("too many launch items".into()));
    }
    output.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for item in items {
        field(output, item.as_bytes())?;
    }
    Ok(())
}

pub fn build_initramfs(
    rootfs: &Path,
    init: &Path,
    busybox: &Path,
    recipe: &[u8],
    output: &Path,
) -> Result<()> {
    build_initramfs_with(rootfs, init, busybox, None, recipe, output)
}

/// As `build_initramfs`, also packaging `mke2fs` for a machine whose
/// services keep named volumes on its retained disk.
pub fn build_initramfs_with(
    rootfs: &Path,
    init: &Path,
    busybox: &Path,
    mke2fs: Option<&Path>,
    recipe: &[u8],
    output: &Path,
) -> Result<()> {
    let mut archive = Newc::new(fs::File::create(output)?);
    archive.owned("seedbox", 0o040755, &[])?;
    archive.owned("init", 0o100755, &fs::read(init)?)?;
    archive.owned("seedbox/busybox", 0o100755, &fs::read(busybox)?)?;
    if let Some(mke2fs) = mke2fs {
        archive.owned("seedbox/mke2fs", 0o100755, &fs::read(mke2fs)?)?;
    }
    archive.owned("seedbox/launch.bin", 0o100400, recipe)?;
    archive.tree(rootfs, "rootfs")?;
    archive.finish()
}

/// Package a Linux-produced tar without extracting Linux ownership, links or
/// case-sensitive paths onto the host filesystem. Both transports use Newc.
pub fn build_initramfs_from_tar(
    rootfs: &Path,
    init: &Path,
    busybox: &Path,
    mke2fs: Option<&Path>,
    recipe: &[u8],
    output: &Path,
) -> Result<()> {
    let mut archive = Newc::new(fs::File::create(output)?);
    archive.owned("seedbox", 0o040755, &[])?;
    archive.owned("init", 0o100755, &fs::read(init)?)?;
    archive.owned("seedbox/busybox", 0o100755, &fs::read(busybox)?)?;
    if let Some(path) = mke2fs {
        archive.owned("seedbox/mke2fs", 0o100755, &fs::read(path)?)?;
    }
    archive.owned("seedbox/launch.bin", 0o100400, recipe)?;
    archive.tar_tree(rootfs)?;
    archive.finish()
}

struct Newc<W> {
    output: W,
    next_inode: u32,
    hardlinks: BTreeMap<(u64, u64), u32>,
}

impl<W: Write> Newc<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            next_inode: 1,
            hardlinks: BTreeMap::new(),
        }
    }

    // These arguments map directly to the fixed newc on-disk header fields.
    #[allow(clippy::too_many_arguments)]
    fn header(
        &mut self,
        name: &str,
        inode: u32,
        mode: u32,
        uid: u32,
        gid: u32,
        nlink: u32,
        mtime: u32,
        size: u32,
    ) -> Result<()> {
        if name.as_bytes().contains(&0)
            || name.starts_with('/')
            || name.split('/').any(|p| p == "..")
        {
            return Err(Error::Execution("invalid initramfs entry name".into()));
        }
        write!(self.output, "070701{inode:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}{mtime:08x}{size:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}", 0, 0, 0, 0, name.len() + 1, 0)?;
        self.output.write_all(name.as_bytes())?;
        self.output.write_all(&[0])?;
        self.padding(110 + name.len() as u64 + 1)
    }

    fn padding(&mut self, count: u64) -> Result<()> {
        self.output
            .write_all(&[0; 3][..((4 - count % 4) % 4) as usize])?;
        Ok(())
    }

    fn owned(&mut self, name: &str, mode: u32, data: &[u8]) -> Result<()> {
        let size = u32::try_from(data.len())
            .map_err(|_| Error::Execution("initramfs file exceeds 4 GiB".into()))?;
        let inode = self.next_inode;
        self.next_inode += 1;
        self.header(name, inode, mode, 0, 0, 1, 0, size)?;
        self.output.write_all(data)?;
        self.padding(size.into())
    }

    fn tree(&mut self, root: &Path, prefix: &str) -> Result<()> {
        for item in walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .follow_links(false)
        {
            let item = item.map_err(|e| Error::Execution(e.to_string()))?;
            let metadata = fs::symlink_metadata(item.path())?;
            // newc cannot encode capabilities, ACLs, or other extended metadata.
            // Refuse to silently change image semantics during packaging.
            if xattr::list(item.path())?.any(|name| {
                // Darwin attaches non-removable provenance to test fixtures.
                // These host annotations are not Linux image attributes.
                !(cfg!(target_os = "macos") && name.as_encoded_bytes().starts_with(b"com.apple."))
            }) {
                return Err(Error::Execution(format!(
                    "image extended attributes unsupported: {}",
                    item.path().display()
                )));
            }
            let relative = item
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .ok_or_else(|| Error::Execution("non-UTF-8 image filename".into()))?;
            let name = if relative.is_empty() {
                prefix.into()
            } else {
                format!("{prefix}/{relative}")
            };
            let inode = self.next_inode;
            self.next_inode += 1;
            let mtime = u32::try_from(metadata.mtime())
                .map_err(|_| Error::Execution("image timestamp exceeds newc range".into()))?;
            if metadata.file_type().is_symlink() {
                let link = fs::read_link(item.path())?;
                let link = link.as_os_str().as_encoded_bytes();
                self.header(
                    &name,
                    inode,
                    metadata.mode(),
                    metadata.uid(),
                    metadata.gid(),
                    1,
                    mtime,
                    link.len() as u32,
                )?;
                self.output.write_all(link)?;
                self.padding(link.len() as u64)?;
            } else if metadata.is_dir() {
                self.header(
                    &name,
                    inode,
                    metadata.mode(),
                    metadata.uid(),
                    metadata.gid(),
                    2,
                    mtime,
                    0,
                )?;
            } else if metadata.is_file() {
                let old_inode = if metadata.nlink() > 1 {
                    self.hardlinks
                        .insert((metadata.dev(), metadata.ino()), inode)
                } else {
                    None
                };
                let archive_inode = old_inode.unwrap_or(inode);
                if old_inode.is_some() {
                    self.hardlinks
                        .insert((metadata.dev(), metadata.ino()), archive_inode);
                }
                let size = if old_inode.is_some() {
                    0
                } else {
                    u32::try_from(metadata.len()).map_err(|_| {
                        Error::Execution("image file exceeds newc 4 GiB limit".into())
                    })?
                };
                self.header(
                    &name,
                    archive_inode,
                    metadata.mode(),
                    metadata.uid(),
                    metadata.gid(),
                    metadata.nlink() as u32,
                    mtime,
                    size,
                )?;
                if size > 0 {
                    let copied = io::copy(
                        &mut fs::File::open(item.path())?.take(size.into()),
                        &mut self.output,
                    )?;
                    if copied != size as u64 {
                        return Err(Error::Integrity("image changed during packaging".into()));
                    }
                }
                self.padding(size.into())?;
            } else {
                return Err(Error::Execution(format!(
                    "special image file unsupported: {}",
                    item.path().display()
                )));
            }
        }
        Ok(())
    }

    fn tar_tree(&mut self, path: &Path) -> Result<()> {
        let invalid = || Error::Integrity("invalid Linux root filesystem archive".into());
        let name = |path: std::path::PathBuf| -> Result<String> {
            let value = path.to_str().ok_or_else(invalid)?.trim_end_matches('/');
            if !(value == "rootfs" || value.starts_with("rootfs/"))
                || path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(invalid());
            }
            Ok(value.to_owned())
        };
        // Resolve hard links first, without buffering file contents. The helper
        // emits sorted tar entries with each hard-link target preceding its links.
        let mut links = BTreeMap::<String, u32>::new();
        let mut seen = std::collections::BTreeSet::new();
        for entry in tar::Archive::new(fs::File::open(path)?).entries()? {
            let entry = entry?;
            let key = name(entry.path()?.into_owned())?;
            if !seen.insert(key.clone()) {
                return Err(invalid());
            }
            if entry.header().entry_type().is_file() {
                links.insert(key, 1);
            }
            if entry.header().entry_type().is_hard_link() {
                let target = name(entry.link_name()?.ok_or_else(invalid)?.into_owned())?;
                let count = links.get_mut(&target).ok_or_else(invalid)?;
                *count = count.checked_add(1).ok_or_else(invalid)?;
            }
        }
        let mut inodes = BTreeMap::new();
        for entry in tar::Archive::new(fs::File::open(path)?).entries()? {
            let mut entry = entry?;
            let key = name(entry.path()?.into_owned())?;
            let header = entry.header();
            let kind = header.entry_type();
            let uid = u32::try_from(header.uid()?).map_err(|_| invalid())?;
            let gid = u32::try_from(header.gid()?).map_err(|_| invalid())?;
            let mtime = u32::try_from(header.mtime()?).map_err(|_| invalid())?;
            let mode = header.mode()? & 0o7777;
            let inode = self.next_inode;
            self.next_inode += 1;
            if kind.is_file() {
                let size = u32::try_from(entry.size()).map_err(|_| invalid())?;
                self.header(
                    &key,
                    inode,
                    mode | 0o100000,
                    uid,
                    gid,
                    links[&key],
                    mtime,
                    size,
                )?;
                inodes.insert(key, inode);
                if io::copy(&mut entry, &mut self.output)? != u64::from(size) {
                    return Err(invalid());
                }
                self.padding(size.into())?;
            } else if kind.is_hard_link() {
                let target = name(entry.link_name()?.ok_or_else(invalid)?.into_owned())?;
                self.header(
                    &key,
                    *inodes.get(&target).ok_or_else(invalid)?,
                    mode | 0o100000,
                    uid,
                    gid,
                    links[&target],
                    mtime,
                    0,
                )?;
            } else if kind.is_dir() {
                self.header(&key, inode, mode | 0o040000, uid, gid, 2, mtime, 0)?;
            } else if kind.is_symlink() {
                let target = entry.link_name()?.ok_or_else(invalid)?.into_owned();
                let bytes = target.as_os_str().as_encoded_bytes();
                let size = u32::try_from(bytes.len()).map_err(|_| invalid())?;
                self.header(&key, inode, mode | 0o120000, uid, gid, 1, mtime, size)?;
                self.output.write_all(bytes)?;
                self.padding(size.into())?;
            } else {
                return Err(invalid());
            }
        }
        if !seen.contains("rootfs") {
            return Err(invalid());
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.owned("TRAILER!!!", 0, &[])?;
        self.output.flush()?;
        Ok(())
    }
}
