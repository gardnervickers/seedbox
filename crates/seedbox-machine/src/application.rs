//! Shared archive preparation. Linux paths remain archive bytes; guest files
//! are never extracted on the host. Backend derivation does not mutate inputs.
use crate::definition::{DefinitionError, FilesystemArchive, Launch, PreparedApplication};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
};
/// Kernel image and its complete caller-supplied argument string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Kernel {
    pub image: Vec<u8>,
    /// Passed verbatim; the backend does not inject or override arguments.
    pub command_line: String,
}
/// Guest bootstrap assets supplied separately from application configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GuestBootstrap {
    pub kernel: Kernel,
    pub init: Vec<u8>,
    /// Optional base archive, represented by an empty vector when absent.
    pub base_initramfs: Vec<u8>,
}

const RECIPE_MAGIC: &[u8; 8] = b"SBXAPP2\0";
const FIELD_LIMIT: usize = 16 * 1024 * 1024;
const LIST_LIMIT: usize = 65536;
#[derive(Clone)]
enum Kind {
    File(Vec<u8>),
    Directory,
    Symlink(Vec<u8>),
    Hardlink(Vec<u8>),
    Character(u32, u32),
    Block(u32, u32),
    Fifo,
}
#[derive(Clone)]
struct Node {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u32,
    kind: Kind,
}
fn invalid(text: impl Into<String>) -> DefinitionError {
    DefinitionError(text.into())
}
fn narrow(n: u64, what: &str) -> Result<u32, DefinitionError> {
    u32::try_from(n).map_err(|_| invalid(format!("{what} exceeds newc range")))
}
fn path(bytes: &[u8]) -> Result<Vec<u8>, DefinitionError> {
    if bytes.contains(&0) || bytes.starts_with(b"/") {
        return Err(invalid(
            "filesystem path must be relative and contain no NUL",
        ));
    }
    let mut out = vec![];
    for part in bytes.split(|b| *b == b'/') {
        if part.is_empty() || part == b"." {
            continue;
        }
        if part == b".." {
            return Err(invalid("filesystem path escapes its root"));
        }
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend(part);
    }
    Ok(out)
}
fn directory() -> Node {
    Node {
        mode: 0o40755,
        uid: 0,
        gid: 0,
        mtime: 0,
        kind: Kind::Directory,
    }
}
fn filesystem(bytes: &[u8]) -> Result<BTreeMap<Vec<u8>, Node>, DefinitionError> {
    let mut nodes = BTreeMap::new();
    let mut archive = tar::Archive::new(bytes);
    for entry in archive
        .entries()
        .map_err(|e| invalid(format!("filesystem tar: {e}")))?
    {
        let mut entry = entry.map_err(|e| invalid(format!("filesystem tar entry: {e}")))?;
        if let Some(attributes) = entry
            .pax_extensions()
            .map_err(|e| invalid(format!("filesystem attributes: {e}")))?
        {
            for attribute in attributes {
                let attribute =
                    attribute.map_err(|e| invalid(format!("filesystem attribute: {e}")))?;
                let key = attribute.key_bytes();
                if key.starts_with(b"SCHILY.xattr.")
                    || key.starts_with(b"LIBARCHIVE.xattr.")
                    || key.starts_with(b"SCHILY.acl.")
                {
                    return Err(invalid(
                        "newc cannot preserve filesystem extended attributes or ACLs",
                    ));
                }
            }
        }
        let name = path(&entry.path_bytes())?;
        let header = entry.header();
        let kind = header.entry_type();
        let permissions = header
            .mode()
            .map_err(|e| invalid(format!("filesystem mode: {e}")))?
            & 0o7777;
        let uid = narrow(
            header
                .uid()
                .map_err(|e| invalid(format!("filesystem UID: {e}")))?,
            "filesystem UID",
        )?;
        let gid = narrow(
            header
                .gid()
                .map_err(|e| invalid(format!("filesystem GID: {e}")))?,
            "filesystem GID",
        )?;
        let mtime = narrow(
            header
                .mtime()
                .map_err(|e| invalid(format!("filesystem mtime: {e}")))?,
            "filesystem mtime",
        )?;
        let (mode, kind) = if kind.is_file() {
            let size = header
                .size()
                .map_err(|e| invalid(format!("filesystem size: {e}")))?;
            narrow(size, "filesystem file")?;
            let mut data = vec![];
            entry
                .read_to_end(&mut data)
                .map_err(|e| invalid(format!("filesystem contents: {e}")))?;
            (0o100000 | permissions, Kind::File(data))
        } else if kind.is_dir() {
            (0o40000 | permissions, Kind::Directory)
        } else if kind.is_symlink() {
            let link = entry
                .link_name_bytes()
                .ok_or_else(|| invalid("symlink target missing"))?
                .to_vec();
            if link.is_empty() || link.contains(&0) {
                return Err(invalid("invalid symlink target"));
            }
            (0o120000 | permissions, Kind::Symlink(link))
        } else if kind.is_hard_link() {
            let link = path(
                &entry
                    .link_name_bytes()
                    .ok_or_else(|| invalid("hardlink target missing"))?,
            )?;
            (0o100000 | permissions, Kind::Hardlink(link))
        } else if kind.is_character_special() || kind.is_block_special() {
            let major = header
                .device_major()
                .map_err(|e| invalid(e.to_string()))?
                .unwrap_or(0);
            let minor = header
                .device_minor()
                .map_err(|e| invalid(e.to_string()))?
                .unwrap_or(0);
            if kind.is_character_special() {
                (0o20000 | permissions, Kind::Character(major, minor))
            } else {
                (0o60000 | permissions, Kind::Block(major, minor))
            }
        } else if kind.is_fifo() {
            (0o10000 | permissions, Kind::Fifo)
        } else {
            return Err(invalid("unsupported filesystem tar entry type"));
        };
        if name.is_empty() && !matches!(kind, Kind::Directory) {
            return Err(invalid("filesystem root must be a directory"));
        }
        // A flattened snapshot has one canonical entry per path (the OCI merge
        // producer already suppresses older duplicates). This is not a layer
        // extraction engine: duplicate hardlink overwrites are ambiguous here.
        if nodes
            .insert(
                name,
                Node {
                    mode,
                    uid,
                    gid,
                    mtime,
                    kind,
                },
            )
            .is_some()
        {
            return Err(invalid(
                "flattened filesystem contains a duplicate canonical path",
            ));
        }
    }
    nodes.entry(vec![]).or_insert_with(directory);
    let names: Vec<_> = nodes.keys().cloned().collect();
    for name in names {
        for (i, &byte) in name.iter().enumerate() {
            if byte != b'/' {
                continue;
            }
            let parent = name[..i].to_vec();
            match nodes.get(&parent) {
                Some(n) if !matches!(n.kind, Kind::Directory) => {
                    return Err(invalid("filesystem entry traverses a non-directory"));
                }
                None => {
                    nodes.insert(parent, directory());
                }
                _ => {}
            }
        }
    }
    for name in nodes.keys() {
        resolve(&nodes, name)?;
    }
    Ok(nodes)
}
fn resolve<'a>(
    nodes: &'a BTreeMap<Vec<u8>, Node>,
    name: &'a [u8],
) -> Result<(&'a [u8], &'a Node), DefinitionError> {
    let mut current = name;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current) {
            return Err(invalid("filesystem hardlink cycle"));
        }
        let node = nodes
            .get(current)
            .ok_or_else(|| invalid("filesystem hardlink target missing"))?;
        match &node.kind {
            Kind::Hardlink(next) => current = next,
            Kind::File(_) => return Ok((current, node)),
            _ if current == name => return Ok((current, node)),
            _ => return Err(invalid("newc hardlinks require a regular-file target")),
        }
    }
}
pub(crate) fn validate_filesystem(bytes: &[u8]) -> Result<(), DefinitionError> {
    filesystem(bytes).map(|_| ())
}
fn text(output: &mut Vec<u8>, value: &str) -> Result<(), DefinitionError> {
    if value.len() > FIELD_LIMIT || value.as_bytes().contains(&0) {
        return Err(invalid("invalid launch text field"));
    }
    output.extend(narrow(value.len() as u64, "launch field")?.to_le_bytes());
    output.extend(value.as_bytes());
    Ok(())
}
fn list(output: &mut Vec<u8>, values: &[String]) -> Result<(), DefinitionError> {
    if values.len() > LIST_LIMIT {
        return Err(invalid("launch list exceeds retained recipe limit"));
    }
    output.extend((values.len() as u32).to_le_bytes());
    for value in values {
        text(output, value)?;
    }
    Ok(())
}
fn recipe(launch: &Launch, prefix: &[String]) -> Result<Vec<u8>, DefinitionError> {
    if prefix.first().is_some_and(|v| v.is_empty()) {
        return Err(invalid("execution prefix executable is empty"));
    }
    if launch.argv.first().is_none_or(|v| v.is_empty()) {
        return Err(invalid("launch executable is empty"));
    }
    if !launch.working_directory.starts_with('/') {
        return Err(invalid("launch working directory must be absolute"));
    }
    let mut environment = vec![];
    for (key, value) in &launch.environment {
        if key.is_empty() || key.contains('=') || key.as_bytes().contains(&0) {
            return Err(invalid("invalid launch environment name"));
        }
        environment.push(format!("{key}={value}"));
    }
    let mut out = RECIPE_MAGIC.to_vec();
    list(&mut out, &launch.argv)?;
    list(&mut out, prefix)?;
    list(&mut out, &environment)?;
    text(&mut out, &launch.working_directory)?;
    text(&mut out, &launch.user)?;
    Ok(out)
}
struct Newc {
    output: Vec<u8>,
    next: u32,
}
impl Newc {
    fn inode(&mut self) -> Result<u32, DefinitionError> {
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("newc inode IDs exhausted"))?;
        Ok(self.next)
    }
    fn entry(
        &mut self,
        name: &[u8],
        inode: u32,
        node: &Node,
        nlink: u32,
        data: &[u8],
    ) -> Result<(), DefinitionError> {
        let size = narrow(data.len() as u64, "newc file")?;
        let namesize = narrow(name.len() as u64 + 1, "newc name")?;
        let (major, minor) = match node.kind {
            Kind::Character(a, b) | Kind::Block(a, b) => (a, b),
            _ => (0, 0),
        };
        write!(self.output,"070701{inode:08x}{:08x}{:08x}{:08x}{nlink:08x}{:08x}{size:08x}{:08x}{:08x}{major:08x}{minor:08x}{namesize:08x}{:08x}",node.mode,node.uid,node.gid,node.mtime,0,0,0).map_err(|e|invalid(e.to_string()))?;
        self.output.extend(name);
        self.output.push(0);
        while !self.output.len().is_multiple_of(4) {
            self.output.push(0);
        }
        self.output.extend(data);
        while !self.output.len().is_multiple_of(4) {
            self.output.push(0);
        }
        Ok(())
    }
    fn owned(&mut self, name: &[u8], mode: u32, data: &[u8]) -> Result<(), DefinitionError> {
        let inode = self.inode()?;
        self.entry(
            name,
            inode,
            &Node {
                mode,
                uid: 0,
                gid: 0,
                mtime: 0,
                kind: Kind::File(vec![]),
            },
            1,
            data,
        )
    }
}
pub(crate) fn boot_archive(
    application: &PreparedApplication,
    bootstrap: &GuestBootstrap,
) -> Result<Vec<u8>, DefinitionError> {
    package(application, bootstrap, &[])
}
pub(crate) struct DerivedApplication {
    pub(crate) application: PreparedApplication,
    pub(crate) execution_prefix: Vec<String>,
}
pub(crate) fn boot_derived(
    application: &DerivedApplication,
    bootstrap: &GuestBootstrap,
) -> Result<Vec<u8>, DefinitionError> {
    package(
        &application.application,
        bootstrap,
        &application.execution_prefix,
    )
}
fn package(
    application: &PreparedApplication,
    bootstrap: &GuestBootstrap,
    prefix: &[String],
) -> Result<Vec<u8>, DefinitionError> {
    if bootstrap.init.is_empty() {
        return Err(invalid("guest bootstrap init is empty"));
    }
    let nodes = filesystem(&application.filesystem.bytes)?;
    let recipe = recipe(&application.launch, prefix)?;
    let mut archive = Newc {
        output: vec![],
        next: 0,
    };
    for name in [b"dev".as_slice(), b"proc", b"sys", b"seedbox"] {
        archive.owned(name, 0o40755, &[])?;
    }
    // Device identity is Linux console 5:1, not a normal empty file.
    let console_inode = archive.inode()?;
    archive.entry(
        b"dev/console",
        console_inode,
        &Node {
            mode: 0o20600,
            uid: 0,
            gid: 0,
            mtime: 0,
            kind: Kind::Character(5, 1),
        },
        1,
        &[],
    )?;
    archive.owned(b"init", 0o100755, &bootstrap.init)?;
    archive.owned(b"seedbox/application.bin", 0o100400, &recipe)?;
    let mut groups: BTreeMap<Vec<u8>, (u32, u32)> = BTreeMap::new();
    for name in nodes.keys() {
        let (key, node) = resolve(&nodes, name)?;
        if matches!(node.kind, Kind::File(_)) {
            let group = groups.entry(key.to_vec()).or_insert((0, 0));
            group.1 = group
                .1
                .checked_add(1)
                .ok_or_else(|| invalid("hardlink count exceeds newc range"))?;
        }
    }
    let mut written = BTreeSet::new();
    for name in nodes.keys() {
        let (key, node) = resolve(&nodes, name)?;
        let mut path = b"rootfs".to_vec();
        if !name.is_empty() {
            path.push(b'/');
            path.extend(name);
        }
        let (inode, nlink, data) = match &node.kind {
            Kind::File(data) => {
                let group = groups.get_mut(key).unwrap();
                if group.0 == 0 {
                    group.0 = archive.inode()?;
                }
                (
                    group.0,
                    group.1,
                    if written.insert(key.to_vec()) {
                        data.as_slice()
                    } else {
                        &[]
                    },
                )
            }
            Kind::Symlink(data) => (archive.inode()?, 1, data.as_slice()),
            _ => (
                archive.inode()?,
                if matches!(node.kind, Kind::Directory) {
                    2
                } else {
                    1
                },
                &[][..],
            ),
        };
        archive.entry(&path, inode, node, nlink, data)?;
    }
    archive.owned(b"TRAILER!!!", 0, &[])?;
    let mut out = bootstrap.base_initramfs.clone();
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out.extend(archive.output);
    Ok(out)
}
/// Derive a complete runtime tree and argv wrapper in a protected namespace.
/// The caller supplies every runtime file (loader/libraries/client), not just a
/// binary. Original workload inputs remain reusable. Read-only guest mounting
/// and actual architecture-specific instrumentation are separate backend work.
pub(crate) fn with_runtime(
    application: &PreparedApplication,
    runtime: &FilesystemArchive,
    prefix: &[String],
) -> Result<DerivedApplication, DefinitionError> {
    let mut nodes = filesystem(&application.filesystem.bytes)?;
    let overlay = filesystem(&runtime.bytes)?;
    let protected = b"seedbox/progress";
    // Preserve links between surviving workload names even when the original
    // inode's first tar name is shadowed by the protected runtime tree.
    let original = nodes.clone();
    let mut surviving: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (name, node) in &mut nodes {
        if name == protected || name.starts_with(b"seedbox/progress/") {
            continue;
        }
        let (key, target) = resolve(&original, name)?;
        if matches!(target.kind, Kind::File(_)) {
            let canonical = surviving
                .entry(key.to_vec())
                .or_insert_with(|| name.clone())
                .clone();
            *node = target.clone();
            if canonical != *name {
                node.kind = Kind::Hardlink(canonical);
            }
        }
    }

    nodes.retain(|path, _| !(path == protected || path.starts_with(b"seedbox/progress/")));
    for (path, node) in overlay {
        if path.is_empty() {
            continue;
        }
        if path == b"seedbox" && matches!(node.kind, Kind::Directory) {
            nodes.entry(path).or_insert(node);
            continue;
        }
        if path != protected && !path.starts_with(b"seedbox/progress/") {
            return Err(invalid(
                "runtime overlay escapes protected helper namespace",
            ));
        }
        nodes.insert(path, node);
    }
    let mut tar = tar::Builder::new(vec![]);
    for (path, node) in nodes {
        let mut header = tar::Header::new_gnu();
        header.set_uid(node.uid.into());
        header.set_gid(node.gid.into());
        header.set_mode(node.mode & 0o7777);
        header.set_mtime(node.mtime.into());
        let (kind, data, link) = match node.kind {
            Kind::File(data) => (tar::EntryType::Regular, data, None),
            Kind::Directory => (tar::EntryType::Directory, vec![], None),
            Kind::Symlink(link) => (tar::EntryType::Symlink, vec![], Some(link)),
            Kind::Hardlink(link) => (tar::EntryType::Link, vec![], Some(link)),
            Kind::Character(a, b) => {
                header
                    .set_device_major(a)
                    .map_err(|e| invalid(e.to_string()))?;
                header
                    .set_device_minor(b)
                    .map_err(|e| invalid(e.to_string()))?;
                (tar::EntryType::Char, vec![], None)
            }
            Kind::Block(a, b) => {
                header
                    .set_device_major(a)
                    .map_err(|e| invalid(e.to_string()))?;
                header
                    .set_device_minor(b)
                    .map_err(|e| invalid(e.to_string()))?;
                (tar::EntryType::Block, vec![], None)
            }
            Kind::Fifo => (tar::EntryType::Fifo, vec![], None),
        };
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        // Header paths can contain Linux bytes even on a non-Linux host.
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::OsStr::from_bytes(if path.is_empty() { b"." } else { &path });
        if let Some(link) = link {
            tar.append_link(&mut header, path, std::ffi::OsStr::from_bytes(&link))
                .map_err(|e| invalid(e.to_string()))?;
        } else {
            tar.append_data(&mut header, path, data.as_slice())
                .map_err(|e| invalid(e.to_string()))?;
        }
    }
    let bytes = tar.into_inner().map_err(|e| invalid(e.to_string()))?;
    recipe(&application.launch, prefix)?;
    Ok(DerivedApplication {
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(bytes)?,
            launch: application.launch.clone(),
        },
        execution_prefix: prefix.to_vec(),
    })
}
#[cfg(test)]
mod tests;
