//! File-only runtime tar reader. Raw iteration is essential: tar's ordinary
//! iterator allocates GNU/PAX metadata and parses sparse maps before yielding.
use super::{allowed, invalid, Result, MEMBER_LIMIT, UNPACKED_LIMIT};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
};

pub(super) const EXTENSION_LIMIT: u64 = 64 << 10;
pub(super) const METADATA_LIMIT: u64 = 1 << 20;
pub(super) const HOST_SYSTEM_LIMIT: u64 = 64;
const PATH_LIMIT: usize = 4096;

#[derive(Clone, Default)]
struct Pax {
    path: Option<String>,
    size: Option<u64>,
}

/// Extract only regular files. Every raw header and its declared body/padding
/// count toward archive quotas before any body is read. GNU long names and PAX
/// metadata have additional per-header/aggregate bounds; links and all sparse
/// representations are rejected without invoking tar's extension parsers.
/// PAX may override paths; a size record must agree with the raw file header,
/// avoiding ambiguous framing. The offline producer uses POSIX pax on both GNU
/// tar and BSD tar, with no sparse-file option and no AppleDouble sidecars.
pub(super) fn extract<R: Read>(archive: &mut tar::Archive<R>, root: &Path) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut headers = 0usize;
    let mut total = 0u64;
    let mut metadata = 0u64;
    let mut global = Pax::default();
    let mut local = None;
    let mut long_name = None;
    for member in archive.entries()?.raw(true) {
        let mut member = member?;
        headers += 1;
        if headers > MEMBER_LIMIT {
            return Err(invalid("driver runtime exceeds 16384 headers"));
        }
        let size = member.size();
        let kind = member.header().entry_type();
        let extension = kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions();
        if extension {
            if size > EXTENSION_LIMIT {
                return Err(invalid("driver runtime extension exceeds 64 KiB"));
            }
            metadata = metadata
                .checked_add(size)
                .filter(|sum| *sum <= METADATA_LIMIT)
                .ok_or_else(|| invalid("driver runtime metadata exceeds 1 MiB"))?;
        }
        let framed = size
            .checked_add(511)
            .map(|size| size & !511)
            .and_then(|size| size.checked_add(512))
            .ok_or_else(|| invalid("driver runtime archive size overflow"))?;
        total = total
            .checked_add(framed)
            .filter(|total| *total <= UNPACKED_LIMIT)
            .ok_or_else(|| invalid("driver runtime exceeds 4 GiB"))?;
        if kind.is_gnu_longlink() {
            return Err(invalid(
                "driver runtime long-link extensions are not allowed",
            ));
        }
        if kind.is_gnu_sparse() {
            return Err(invalid("driver runtime sparse files are not allowed"));
        }
        if kind.is_gnu_longname() {
            if long_name.is_some() {
                return Err(invalid("duplicate driver runtime long-name extension"));
            }
            let bytes = extension_bytes(&mut member, size)?;
            let path = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
            long_name = Some(path_string(path)?);
            continue;
        }
        if kind.is_pax_local_extensions() {
            if local.is_some() {
                return Err(invalid("duplicate driver runtime local PAX extension"));
            }
            let mut fields = global.clone();
            parse_pax(&extension_bytes(&mut member, size)?, &mut fields)?;
            local = Some(fields);
            continue;
        }
        if kind.is_pax_global_extensions() {
            if local.is_some() || long_name.is_some() {
                return Err(invalid(
                    "global PAX extension interrupts pending runtime member metadata",
                ));
            }
            parse_pax(&extension_bytes(&mut member, size)?, &mut global)?;
            continue;
        }
        if !kind.is_file() {
            return Err(invalid("driver runtime requires regular file members"));
        }
        let fields = local.take().unwrap_or_else(|| global.clone());
        if fields.size.is_some_and(|declared| declared != size) {
            return Err(invalid("driver runtime PAX size conflicts with raw header"));
        }
        let name = match fields.path.or_else(|| long_name.clone()) {
            Some(path) => path,
            None => path_string(&member.header().path_bytes())?,
        };
        long_name = None;
        if !allowed(&name) || !seen.insert(name.clone()) {
            return Err(invalid(format!(
                "invalid or duplicate driver runtime member {name:?}"
            )));
        }
        if name == "host-system" && size > HOST_SYSTEM_LIMIT {
            return Err(invalid("driver runtime host-system exceeds 64 bytes"));
        }
        let target = root.join(&name);
        fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| invalid("runtime member parent"))?,
        )?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let length = std::io::copy(&mut member, &mut file)?;
        if length != size {
            return Err(invalid(format!("truncated driver runtime member {name}")));
        }
        file.flush()?;
        let mode = if member.header().mode()? & 0o111 != 0 {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
    }
    if local.is_some() || long_name.is_some() {
        return Err(invalid("driver runtime extension has no following file"));
    }
    Ok(())
}

fn extension_bytes(reader: &mut impl Read, size: u64) -> Result<Vec<u8>> {
    // Called only after the declared size and aggregate metadata/header budgets
    // are checked. No tar helper may auto-read an extension before this point.
    let mut bytes = Vec::with_capacity(size as usize);
    reader.take(size).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size {
        return Err(invalid("truncated driver runtime extension"));
    }
    Ok(bytes)
}
fn path_string(bytes: &[u8]) -> Result<String> {
    if bytes.len() > PATH_LIMIT {
        return Err(invalid("driver runtime path exceeds 4096 bytes"));
    }
    if bytes.contains(&0) {
        return Err(invalid("NUL in driver runtime path"));
    }
    Ok(std::str::from_utf8(bytes)
        .map_err(|_| invalid("non-UTF-8 driver runtime path"))?
        .to_owned())
}
fn parse_pax(mut bytes: &[u8], fields: &mut Pax) -> Result<()> {
    let mut path_seen = false;
    let mut size_seen = false;
    while !bytes.is_empty() {
        let separator = bytes
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| invalid("invalid runtime PAX record length"))?;
        let length = decimal(&bytes[..separator])?;
        let length =
            usize::try_from(length).map_err(|_| invalid("runtime PAX record length overflow"))?;
        if length <= separator + 1 || length > bytes.len() || bytes[length - 1] != b'\n' {
            return Err(invalid("invalid runtime PAX record length"));
        }
        let record = &bytes[separator + 1..length - 1];
        let equals = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| invalid("invalid runtime PAX record"))?;
        let (key, value) = (&record[..equals], &record[equals + 1..]);
        if key.starts_with(b"GNU.sparse.")
            || key == b"SCHILY.realsize"
            || (key == b"SCHILY.filetype" && value == b"sparse")
        {
            return Err(invalid("driver runtime PAX sparse files are not allowed"));
        }
        match key {
            b"path" => {
                if path_seen {
                    return Err(invalid("duplicate runtime PAX path"));
                }
                path_seen = true;
                fields.path = if value.is_empty() {
                    None
                } else {
                    Some(path_string(value)?)
                };
            }
            b"size" => {
                if size_seen {
                    return Err(invalid("duplicate runtime PAX size"));
                }
                size_seen = true;
                fields.size = if value.is_empty() {
                    None
                } else {
                    Some(decimal(value)?)
                };
            }
            b"linkpath" => return Err(invalid("driver runtime PAX link paths are not allowed")),
            _ => {} // Bounded ownership/timestamps/xattrs do not affect extraction.
        }
        bytes = &bytes[length..];
    }
    Ok(())
}
fn decimal(bytes: &[u8]) -> Result<u64> {
    if bytes.is_empty() {
        return Err(invalid("empty runtime PAX integer"));
    }
    bytes.iter().try_fold(0u64, |value, byte| {
        if !byte.is_ascii_digit() {
            return Err(invalid("invalid runtime PAX integer"));
        }
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add((byte - b'0') as u64))
            .ok_or_else(|| invalid("runtime PAX integer overflow"))
    })
}
