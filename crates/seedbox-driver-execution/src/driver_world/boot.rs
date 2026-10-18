//! Complete boot authority and its derived logical filesystem view. Import does
//! not extract guest paths on the host or alter the complete bytes being launched.
#[cfg(test)]
mod tests;
use crate::{Error, Result};
use seedbox_engine_protocol::driver_world::Artifact;
use std::{collections::BTreeMap, fs, io::Write, os::unix::ffi::OsStrExt, path::Path};

fn bad(message: impl Into<String>) -> Error {
    Error::Integrity(message.into())
}
pub(super) fn complete(bytes: &[u8]) -> bool {
    bytes.starts_with(b"070701") || bytes.starts_with(b"070702")
}
pub(super) fn minimum_memory(archive_len: u64) -> Result<u64> {
    archive_len
        .checked_mul(3)
        .and_then(|bytes| bytes.checked_add(128 * 1024 * 1024))
        .ok_or_else(|| bad("complete boot RAM sizing overflow"))
}
#[derive(Clone)]
struct Node {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u32,
    dev: (u32, u32),
    data: Vec<u8>,
    inode: Option<(u64, u32, u32, u32)>,
}
fn take<'a>(bytes: &'a [u8], cursor: &mut usize, count: usize) -> Result<&'a [u8]> {
    let end = cursor
        .checked_add(count)
        .ok_or_else(|| bad("newc offset overflow"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| bad("truncated complete newc boot archive"))?;
    *cursor = end;
    Ok(value)
}
fn align(bytes: &[u8], cursor: &mut usize) -> Result<()> {
    let n = (4 - (*cursor % 4)) % 4;
    take(bytes, cursor, n)?;
    Ok(())
}
pub(super) fn filesystem(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = 0;
    let mut archive = 0u64;
    let mut nodes = BTreeMap::new();
    let mut links = BTreeMap::new();
    while cursor < bytes.len() {
        while bytes.get(cursor) == Some(&0) {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }
        let header = take(bytes, &mut cursor, 110)?;
        let crc = match &header[..6] {
            b"070701" => false,
            b"070702" => true,
            _ => return Err(bad("complete boot must be an uncompressed newc archive")),
        };
        let field = |index: usize| -> Result<u32> {
            let begin = 6 + index * 8;
            u32::from_str_radix(
                std::str::from_utf8(&header[begin..begin + 8])
                    .map_err(|_| bad("invalid newc hexadecimal field"))?,
                16,
            )
            .map_err(|_| bad("invalid newc hexadecimal field"))
        };
        let size = field(6)? as usize;
        let name_len = field(11)? as usize;
        let name = take(bytes, &mut cursor, name_len)?;
        if name.last() != Some(&0) || name.len() < 2 {
            return Err(bad("invalid complete boot newc path"));
        }
        let name = &name[..name.len() - 1];
        align(bytes, &mut cursor)?;
        let data = take(bytes, &mut cursor, size)?;
        align(bytes, &mut cursor)?;
        if crc
            && data
                .iter()
                .fold(0u32, |sum, b| sum.wrapping_add(u32::from(*b)))
                != field(12)?
        {
            return Err(bad("complete boot newc checksum mismatch"));
        }
        if name == b"TRAILER!!!" {
            archive = archive
                .checked_add(1)
                .ok_or_else(|| bad("newc archive count overflow"))?;
            continue;
        }
        let mode = field(1)?;
        let inode = (mode & 0o170000 == 0o100000 && field(4)? > 1).then_some((
            archive,
            field(7)?,
            field(8)?,
            field(0)?,
        ));
        if let Some(key) = inode {
            if !data.is_empty() || !links.contains_key(&key) {
                links.insert(key, data.to_vec());
            }
        }
        nodes.insert(
            name.to_vec(),
            Node {
                mode,
                uid: field(2)?,
                gid: field(3)?,
                mtime: field(5)?,
                dev: (field(9)?, field(10)?),
                data: data.to_vec(),
                inode,
            },
        );
    }
    let mut tar = tar::Builder::new(Vec::new());
    let mut targets = BTreeMap::<(u64, u32, u32, u32), Vec<u8>>::new();
    for (name, node) in nodes {
        let mut header = tar::Header::new_gnu();
        header.set_mode(node.mode & 0o7777);
        header.set_uid(u64::from(node.uid));
        header.set_gid(u64::from(node.gid));
        header.set_mtime(u64::from(node.mtime));
        let data = node
            .inode
            .and_then(|key| links.get(&key))
            .unwrap_or(&node.data);
        let kind = match node.mode & 0o170000 {
            0o100000 => tar::EntryType::Regular,
            0o040000 => tar::EntryType::Directory,
            0o120000 => tar::EntryType::Symlink,
            0o020000 => tar::EntryType::Char,
            0o060000 => tar::EntryType::Block,
            0o010000 => tar::EntryType::Fifo,
            _ => return Err(bad("unsupported Linux file type in complete boot")),
        };
        header.set_entry_type(kind);
        header.set_size(if kind.is_file() { data.len() as u64 } else { 0 });
        let path = std::ffi::OsStr::from_bytes(&name);
        if let Some(key) = node.inode {
            if let Some(target) = targets.get(&key) {
                header.set_entry_type(tar::EntryType::Link);
                header.set_size(0);
                tar.append_link(&mut header, path, std::ffi::OsStr::from_bytes(target))?;
                continue;
            }
            targets.insert(key, name.clone());
        }
        if kind.is_symlink() {
            tar.append_link(&mut header, path, std::ffi::OsStr::from_bytes(data))?;
        } else {
            if kind.is_character_special() || kind.is_block_special() {
                header.set_device_major(node.dev.0)?;
                header.set_device_minor(node.dev.1)?;
            }
            tar.append_data(
                &mut header,
                path,
                if kind.is_file() { data.as_slice() } else { &[] },
            )?;
        }
    }
    Ok(tar.into_inner()?)
}
pub(super) fn retain_view(bytes: &[u8], output: &Path) -> Result<Artifact> {
    fs::create_dir_all(output)?;
    let data = filesystem(bytes)?;
    let mut file = tempfile::Builder::new()
        .prefix("driver-boot-view-")
        .suffix(".tar")
        .tempfile_in(output)?;
    file.write_all(&data)?;
    file.as_file().sync_all()?;
    let (_, path) = file.keep().map_err(|error| {
        bad(format!(
            "retain complete boot filesystem view: {}",
            error.error
        ))
    })?;
    Ok(Artifact {
        sha256: crate::store::ContentDigest::of(&data).hex().into(),
        length: data.len() as u64,
        artifact: path
            .to_str()
            .ok_or_else(|| bad("boot view path is not UTF-8"))?
            .into(),
    })
}
