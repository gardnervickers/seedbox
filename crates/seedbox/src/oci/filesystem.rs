//! Apply verified OCI layers without creating image paths on the host.
//!
//! Linux metadata stays in memory; file contents use private numbered spools.
//! The guest archive writer consumes this tree directly, without chown, chmod,
//! symlink traversal on the host, or a privileged unpacker.

use crate::{Error, Result};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const MAX_ENTRIES: usize = 1_000_000;

#[derive(Debug)]
pub(crate) enum Contents {
    Directory,
    File { path: PathBuf, size: u32 },
    Symlink(Vec<u8>),
}

#[derive(Debug)]
pub(crate) struct Node {
    pub inode: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    pub contents: Contents,
}

/// A Linux filesystem whose ownership and links do not depend on the host OS.
/// The caller must keep its private spool directory alive until packaging ends.
#[derive(Debug)]
pub struct ImageFilesystem {
    pub(crate) entries: BTreeMap<String, Arc<Node>>,
    directory: PathBuf,
    next_inode: u32,
}

enum Change {
    Node(Arc<Node>),
    Hardlink(String),
}

impl ImageFilesystem {
    pub fn new(directory: &Path) -> Result<Self> {
        fs::create_dir(directory)?;
        let mut tree = Self {
            entries: BTreeMap::new(),
            directory: directory.into(),
            next_inode: 1,
        };
        tree.directory_entry("")?;
        Ok(tree)
    }

    fn inode(&mut self) -> Result<u32> {
        let inode = self.next_inode;
        self.next_inode = inode
            .checked_add(1)
            .ok_or_else(|| Error::Integrity("too many image inodes".into()))?;
        Ok(inode)
    }

    fn directory_entry(&mut self, path: &str) -> Result<()> {
        let node = Node {
            inode: self.inode()?,
            mode: 0o040755,
            uid: 0,
            gid: 0,
            mtime: 0,
            contents: Contents::Directory,
        };
        self.entries.insert(path.into(), Arc::new(node));
        Ok(())
    }

    /// Apply an uncompressed, digest-verified layer. Whiteouts affect only the
    /// previous tree, including when they occur after new entries in the tar.
    /// Cancellation is checked between headers and each 64 KiB of file data.
    pub fn apply_layer(&mut self, input: impl Read, check: &dyn Fn() -> Result<()>) -> Result<()> {
        let input = super::tar_reader::TarReader::new(input, 16 * 1024 * 1024 * 1024, check);
        let mut archive = tar::Archive::new(input);
        let mut changes = Vec::new();
        let mut whiteouts = Vec::new();
        let mut seen = BTreeSet::new();
        for item in archive.entries()? {
            check()?;
            let mut entry = item?;
            let path = archive_path(&entry.path()?)?;
            if !seen.insert(path.clone()) {
                return Err(Error::Integrity(format!("duplicate layer path: {path}")));
            }
            if seen.len() > MAX_ENTRIES {
                return Err(Error::Integrity("layer exceeds one million entries".into()));
            }
            let mut pax_mtime = None;
            let mut pax_uid = None;
            let mut pax_gid = None;
            if let Some(extensions) = entry.pax_extensions()? {
                let mut metadata_keys = BTreeSet::new();
                for extension in extensions {
                    let extension = extension?;
                    let key = extension
                        .key()
                        .map_err(|_| Error::Integrity("non-UTF-8 image metadata key".into()))?;
                    if !metadata_keys.insert(key.to_owned()) {
                        return Err(Error::Integrity(format!(
                            "duplicate image metadata {key} at {path}"
                        )));
                    }
                    match key {
                        "mtime" => {
                            // newc stores whole seconds, as the previous importer
                            // did. Do not substitute the host extraction time.
                            let value = extension
                                .value()
                                .map_err(|_| Error::Integrity("non-UTF-8 image mtime".into()))?;
                            let seconds = match value.split_once('.') {
                                Some((seconds, fraction)) => {
                                    if fraction.is_empty()
                                        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
                                    {
                                        return Err(Error::Integrity(
                                            "invalid image PAX mtime fraction".into(),
                                        ));
                                    }
                                    seconds
                                }
                                None => value,
                            };
                            pax_mtime = Some(pax_number(seconds, "mtime")?);
                        }
                        "uid" | "gid" => {
                            let value = extension.value().map_err(|_| {
                                Error::Integrity(format!("non-UTF-8 image PAX {key}"))
                            })?;
                            let value = pax_number(value, key)?;
                            if key == "uid" {
                                pax_uid = Some(value);
                            } else {
                                pax_gid = Some(value);
                            }
                        }
                        "path" | "linkpath" | "size" | "uname" | "gname" | "atime" | "ctime" => (),
                        _ => {
                            return Err(Error::Integrity(format!(
                                "unsupported image metadata {key} at {path}"
                            )))
                        }
                    }
                }
            }
            let header = entry.header();
            let kind = header.entry_type();
            let basename = path.rsplit('/').next().unwrap_or("");
            if let Some(deleted) = basename.strip_prefix(".wh.") {
                if !kind.is_file() || entry.size() != 0 || ["", ".", ".."].contains(&deleted) {
                    return Err(Error::Integrity(format!("invalid whiteout: {path}")));
                }
                let parent = parent(&path);
                whiteouts.push((
                    join(parent, deleted),
                    basename == ".wh..wh..opq",
                    parent.to_owned(),
                ));
                continue;
            }
            if path.is_empty() && !kind.is_dir() {
                return Err(Error::Integrity("image root must be a directory".into()));
            }
            if kind.is_hard_link() {
                let target = entry
                    .link_name()?
                    .ok_or_else(|| Error::Integrity(format!("hardlink without target: {path}")))?;
                changes.push((path, Change::Hardlink(archive_path(&target)?)));
                continue;
            }
            let inode = self.inode()?;
            let mode = header.mode()? & 0o7777;
            let uid = match pax_uid {
                Some(value) => value,
                None => newc_number(header.uid()?, "uid")?,
            };
            let gid = match pax_gid {
                Some(value) => value,
                None => newc_number(header.gid()?, "gid")?,
            };
            let mtime = match pax_mtime {
                Some(value) => value,
                None => newc_number(header.mtime()?, "mtime")?,
            };
            let (mode, contents) = if kind.is_dir() {
                if entry.size() != 0 {
                    return Err(Error::Integrity(format!("directory has data: {path}")));
                }
                (mode | 0o040000, Contents::Directory)
            } else if kind.is_symlink() {
                if entry.size() != 0 {
                    return Err(Error::Integrity(format!("symlink has data: {path}")));
                }
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| Error::Integrity(format!("symlink without target: {path}")))?;
                if target.is_empty() || target.contains(&0) || target.len() >= 4096 {
                    return Err(Error::Integrity(format!("invalid symlink: {path}")));
                }
                (mode | 0o120000, Contents::Symlink(target.into_owned()))
            } else if kind.is_file() {
                let size = newc_number(entry.size(), "file size")?;
                let data = self.directory.join(inode.to_string());
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&data)?;
                let copied = copy_checked(&mut entry, &mut output, size.into(), check)?;
                if copied != u64::from(size) {
                    return Err(Error::Integrity(format!("truncated image file: {path}")));
                }
                (mode | 0o100000, Contents::File { path: data, size })
            } else {
                return Err(Error::Integrity(format!(
                    "unsupported special or sparse image file: {path}"
                )));
            };
            changes.push((
                path,
                Change::Node(Arc::new(Node {
                    inode,
                    mode,
                    uid,
                    gid,
                    mtime,
                    contents,
                })),
            ));
        }
        for (path, opaque, parent) in whiteouts {
            check()?;
            if opaque {
                let path = self.resolve(&parent, true)?;
                self.remove(&path, false);
            } else {
                let path = self.resolve(&path, false)?;
                self.remove(&path, true);
            }
        }
        let mut hardlinks = Vec::new();
        let mut destinations = BTreeSet::new();
        for (path, change) in changes {
            check()?;
            let path = self.resolve(&path, false)?;
            if !destinations.insert(path.clone()) {
                return Err(Error::Integrity(format!("aliased layer path: {path}")));
            }
            self.parents(&path)?;
            match change {
                Change::Node(node) => {
                    let both_directories = matches!(node.contents, Contents::Directory)
                        && self
                            .entries
                            .get(&path)
                            .is_some_and(|old| matches!(old.contents, Contents::Directory));
                    if !both_directories {
                        if hardlinks.iter().any(|(destination, _): &(String, String)| {
                            destination == &path || destination.starts_with(&format!("{path}/"))
                        }) {
                            return Err(Error::Integrity(format!(
                                "image replaces the parent of a deferred hardlink: {path}"
                            )));
                        }
                        self.remove(&path, true);
                    }
                    self.entries.insert(path, node);
                }
                Change::Hardlink(target) => {
                    self.remove(&path, true);
                    let target = self.resolve(&target, false)?;
                    if let Some(node) = self.entries.get(&target) {
                        if !matches!(node.contents, Contents::File { .. }) {
                            return Err(Error::Integrity(format!(
                                "hardlink to a non-regular image file: {target}"
                            )));
                        }
                        self.entries.insert(path, Arc::clone(node));
                    } else {
                        hardlinks.push((path, target));
                    }
                }
            }
        }
        // Resolve forward references too, without losing a lower-layer inode
        // when its other directory entry is replaced by a new file.
        while !hardlinks.is_empty() {
            let count = hardlinks.len();
            let mut pending = Vec::new();
            for (path, target) in hardlinks {
                check()?;
                self.validate_parents(&path)?;
                let target = self.resolve(&target, false)?;
                match self.entries.get(&target) {
                    Some(node) if matches!(node.contents, Contents::File { .. }) => {
                        self.entries.insert(path, Arc::clone(node));
                    }
                    Some(_) => {
                        return Err(Error::Integrity(format!(
                            "hardlink to a non-regular image file: {target}"
                        )))
                    }
                    None => pending.push((path, target)),
                }
            }
            if pending.len() == count {
                return Err(Error::Integrity(
                    "missing or cyclic image hardlink target".into(),
                ));
            }
            hardlinks = pending;
        }
        if self.entries.len() > MAX_ENTRIES {
            return Err(Error::Integrity("image exceeds one million entries".into()));
        }
        self.validate()?;
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if !self
            .entries
            .get("")
            .is_some_and(|node| matches!(node.contents, Contents::Directory))
        {
            return Err(Error::Integrity("image root must be a directory".into()));
        }
        for path in self.entries.keys() {
            archive_path(Path::new(path))?;
            self.validate_parents(path)?;
        }
        Ok(())
    }

    fn validate_parents(&self, path: &str) -> Result<()> {
        let mut ancestor = parent(path);
        loop {
            if !self
                .entries
                .get(ancestor)
                .is_some_and(|node| matches!(node.contents, Contents::Directory))
            {
                return Err(Error::Integrity(format!(
                    "image parent is not a directory: {ancestor}"
                )));
            }
            if ancestor.is_empty() {
                return Ok(());
            }
            ancestor = parent(ancestor);
        }
    }

    fn remove(&mut self, path: &str, include_root: bool) {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        self.entries.retain(|name, _| {
            !((include_root && name == path) || (name != path && name.starts_with(&prefix)))
        });
    }

    fn parents(&mut self, path: &str) -> Result<()> {
        let mut directory = parent(path);
        let mut missing = Vec::new();
        loop {
            match self.entries.get(directory) {
                Some(node) if matches!(node.contents, Contents::Directory) => break,
                Some(_) => {
                    return Err(Error::Integrity(format!(
                        "image parent is not a directory: {directory}"
                    )))
                }
                None => missing.push(directory.to_owned()),
            }
            if directory.is_empty() {
                break;
            }
            directory = parent(directory);
        }
        for directory in missing.into_iter().rev() {
            self.directory_entry(&directory)?;
        }
        Ok(())
    }

    /// Resolve symlinks in guest namespace only. Absolute link targets are
    /// rooted in the image, and .. at its root stays at its root (chroot rules).
    fn resolve(&self, path: &str, follow_final: bool) -> Result<String> {
        let mut pending: VecDeque<String> = path.split('/').map(str::to_owned).collect();
        let mut resolved = Vec::new();
        let mut links = 0;
        while let Some(part) = pending.pop_front() {
            match part.as_str() {
                "" | "." => continue,
                ".." => {
                    resolved.pop();
                    continue;
                }
                _ => (),
            }
            let candidate = join(&resolved.join("/"), &part);
            if follow_final || !pending.is_empty() {
                if let Some(Node {
                    contents: Contents::Symlink(target),
                    ..
                }) = self.entries.get(&candidate).map(Arc::as_ref)
                {
                    links += 1;
                    if links > 40 {
                        return Err(Error::Integrity(format!("image symlink cycle: {path}")));
                    }
                    let target = std::str::from_utf8(target)
                        .map_err(|_| Error::Integrity("non-UTF-8 image link target".into()))?;
                    if target.starts_with('/') {
                        resolved.clear();
                    }
                    for component in target.split('/').rev() {
                        pending.push_front(component.into());
                    }
                    continue;
                }
            }
            resolved.push(part);
        }
        archive_path(Path::new(&resolved.join("/")))
    }
}

pub(crate) fn archive_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::CurDir => (),
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| Error::Integrity("non-UTF-8 image filename".into()))?;
                if part.contains('\0') || part.len() > 255 {
                    return Err(Error::Integrity(
                        "NUL or oversized component in image filename".into(),
                    ));
                }
                parts.push(part);
            }
            _ => {
                return Err(Error::Integrity(format!(
                    "unsafe archive path: {}",
                    path.display()
                )))
            }
        }
    }
    let path = parts.join("/");
    if path.len() + "rootfs/".len() + 1 > 4096 {
        return Err(Error::Integrity(
            "image filename exceeds Linux path limit".into(),
        ));
    }
    Ok(path)
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    }
}

fn newc_number(value: u64, field: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::Integrity(format!("image {field} exceeds newc range")))
}

fn pax_number(value: &str, field: &str) -> Result<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Integrity(format!("invalid image PAX {field}")));
    }
    value
        .parse::<u32>()
        .map_err(|_| Error::Integrity(format!("image PAX {field} exceeds newc range")))
}

pub(crate) fn copy_checked(
    input: &mut impl Read,
    output: &mut impl Write,
    limit: u64,
    check: &dyn Fn() -> Result<()>,
) -> Result<u64> {
    let mut buffer = [0; 64 * 1024];
    let mut total = 0u64;
    loop {
        check()?;
        let result = input.read(&mut buffer);
        check()?;
        let count = result?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .filter(|total| *total <= limit)
            .ok_or_else(|| Error::Integrity(format!("image input exceeds {limit} bytes")))?;
        output.write_all(&buffer[..count])?;
    }
    check()?;
    Ok(total)
}
