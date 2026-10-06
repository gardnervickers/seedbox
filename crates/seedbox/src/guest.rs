//! Deterministic initramfs assembly around an imported image filesystem.
//!
//! Image file metadata and hard links are preserved; only Seedbox-owned bootstrap
//! files have normalized timestamps. Each distinct image is packed once; each
//! service runs chrooted in its own writable overlay of it, with its own mount,
//! PID, UTS and (when services share a kernel) network namespaces.

use crate::{image::Launch, Error, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

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

/// How a machine's services reach the simulated network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestNetwork {
    /// One service uses the machine's virtio NIC (`eth0`) in the root network
    /// namespace; the engine's fabric carries its packets.
    Device,
    /// Each service has its own network namespace joined to the root
    /// namespace by a veth pair. The root namespace routes between them and
    /// delays each packet once, on egress towards its destination.
    Namespaces { latency_ns: u64, jitter_ns: u64 },
}

/// One service in a machine's launch recipe.
pub struct GuestService<'a> {
    pub name: &'a str,
    /// Directory under `images/` holding its read-only image root.
    pub image: &'a str,
    /// IPv4 address on the shared /24 service network.
    pub address: &'a str,
    pub launch: &'a Launch,
    pub configs: &'a BTreeMap<String, Vec<u8>>,
    /// Its exit status completes the machine (and the run, if it is the check).
    pub check: bool,
    /// Receives the runtime control socket.
    pub controller: bool,
}

/// This binary recipe is consumed by the small static Linux guest init. Length
/// prefixes avoid shell evaluation of command, environment or config contents.
pub fn launch_recipe(
    network: GuestNetwork,
    hosts: &str,
    services: &[GuestService<'_>],
) -> Result<Vec<u8>> {
    let mut bytes = b"SBXLAUNCH2".to_vec();
    let (mode, latency, jitter) = match network {
        GuestNetwork::Device => (0u32, 0, 0),
        GuestNetwork::Namespaces {
            latency_ns,
            jitter_ns,
        } => (1, latency_ns, jitter_ns),
    };
    if mode == 0 && services.len() != 1 {
        return Err(Error::Execution(
            "a machine NIC serves exactly one service".into(),
        ));
    }
    if services.is_empty() || services.len() > 254 {
        return Err(Error::Execution("expected 1 to 254 services".into()));
    }
    bytes.extend_from_slice(&mode.to_le_bytes());
    bytes.extend_from_slice(&latency.to_le_bytes());
    bytes.extend_from_slice(&jitter.to_le_bytes());
    field(&mut bytes, hosts.as_bytes())?;
    bytes.extend_from_slice(&(services.len() as u32).to_le_bytes());
    for service in services {
        let flags = u32::from(service.check) | u32::from(service.controller) << 1;
        field(&mut bytes, service.name.as_bytes())?;
        field(&mut bytes, service.image.as_bytes())?;
        field(&mut bytes, service.address.as_bytes())?;
        bytes.extend_from_slice(&flags.to_le_bytes());
        let launch = service.launch;
        for value in [&launch.user, &launch.working_dir] {
            field(&mut bytes, value.as_bytes())?;
        }
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
        if service.configs.len() > 65536 {
            return Err(Error::Execution("too many config mounts".into()));
        }
        bytes.extend_from_slice(&(service.configs.len() as u32).to_le_bytes());
        for (target, content) in service.configs {
            field(&mut bytes, target.as_bytes())?;
            field(&mut bytes, content)?;
        }
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

/// Package the supervisor, its recipe, one read-only root per distinct image
/// (`images/<id>`) and an empty mount point per service (`services/<name>`),
/// where the guest mounts that service's overlay of its image.
pub fn build_initramfs(
    images: &[(&str, &Path)],
    services: &[&str],
    init: &Path,
    busybox: &Path,
    recipe: &[u8],
    output: &Path,
) -> Result<()> {
    let mut archive = Newc::new(fs::File::create(output)?);
    archive.owned("seedbox", 0o040755, &[])?;
    archive.owned("init", 0o100755, &fs::read(init)?)?;
    archive.owned("seedbox/busybox", 0o100755, &fs::read(busybox)?)?;
    archive.owned("seedbox/launch.bin", 0o100400, recipe)?;
    archive.owned("images", 0o040755, &[])?;
    for (id, rootfs) in images {
        archive.tree(rootfs, &format!("images/{id}"))?;
    }
    archive.owned("services", 0o040755, &[])?;
    for name in services {
        archive.owned(&format!("services/{name}"), 0o040755, &[])?;
    }
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
        // Hard links never cross image roots.
        self.hardlinks.clear();
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

    fn finish(mut self) -> Result<()> {
        self.owned("TRAILER!!!", 0, &[])?;
        self.output.flush()?;
        Ok(())
    }
}
