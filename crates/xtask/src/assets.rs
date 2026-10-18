//! Compare prepared bundles without treating archive timestamps as asset changes.
use super::Result;
use flate2::read::MultiGzDecoder;
use std::{
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn members(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(io::Error::other("asset bundle contains a non-file"));
        }
        names.push(entry.file_name().into());
    }
    names.sort();
    Ok(names)
}

fn bytes_equal(mut a: impl Read, mut b: impl Read, mut length: u64) -> io::Result<bool> {
    let mut left = [0; 65536];
    let mut right = [0; 65536];
    while length != 0 {
        let size = length.min(left.len() as u64) as usize;
        a.read_exact(&mut left[..size])?;
        b.read_exact(&mut right[..size])?;
        if left[..size] != right[..size] {
            return Ok(false);
        }
        length -= size as u64;
    }
    Ok(true)
}

fn archives_equal(a: &Path, b: &Path) -> Result<bool> {
    let mut a = tar::Archive::new(MultiGzDecoder::new(File::open(a)?));
    let mut b = tar::Archive::new(MultiGzDecoder::new(File::open(b)?));
    let mut left = a.entries()?;
    let mut right = b.entries()?;
    loop {
        let (mut a, mut b) = match (left.next(), right.next()) {
            (None, None) => break,
            (Some(a), Some(b)) => (a?, b?),
            _ => return Ok(false),
        };
        // Preserve member order and all runtime-relevant metadata. PAX timestamp
        // extensions and gzip headers are packaging details, not guest inputs.
        if a.path()? != b.path()?
            || a.header().entry_type() != b.header().entry_type()
            || a.header().mode()? != b.header().mode()?
            || a.header().uid()? != b.header().uid()?
            || a.header().gid()? != b.header().gid()?
            || a.link_name()? != b.link_name()?
            || a.size() != b.size()
        {
            return Ok(false);
        }
        let size = a.size();
        if !bytes_equal(&mut a, &mut b, size)? {
            return Ok(false);
        }
    }
    // Match the Driver runtime reader's completeness check: tar stops before
    // gzip validates its trailer. Accept only bounded zero padding after tar,
    // including across concatenated gzip members, and validate every trailer.
    Ok(complete(a.into_inner())? && complete(b.into_inner())?)
}

fn complete(mut decoder: impl Read) -> io::Result<bool> {
    let mut padding = [0; 8192];
    let mut size = 0;
    loop {
        let read = decoder.read(&mut padding)?;
        if read == 0 {
            return Ok(true);
        }
        size += read;
        if size > (1 << 20) || padding[..read].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
    }
}

pub(super) fn equal(a: &Path, b: &Path) -> Result<bool> {
    let names = members(a)?;
    if names != members(b)? {
        return Ok(false);
    }
    for name in names {
        let a = a.join(&name);
        let b = b.join(&name);
        let left = fs::metadata(&a)?;
        let right = fs::metadata(&b)?;
        if left.permissions().mode() != right.permissions().mode() {
            return Ok(false);
        }
        if name == Path::new("runtime.tar.gz") || name == Path::new("driver-runtime.tar.gz") {
            if !archives_equal(&a, &b)? {
                return Ok(false);
            }
        } else if left.len() != right.len()
            || !bytes_equal(File::open(a)?, File::open(b)?, left.len())?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression, GzBuilder};

    pub(crate) fn archive(path: &Path, bytes: &[u8], mode: u32, time: u32) {
        let gzip: GzEncoder<_> = GzBuilder::new()
            .mtime(time)
            .write(File::create(path).unwrap(), Compression::fast());
        let mut archive = tar::Builder::new(gzip);
        let time_text = time.to_string();
        archive
            .append_pax_extensions([("mtime", time_text.as_bytes())])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        header.set_mtime(time.into());
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        archive.append_data(&mut header, "init", bytes).unwrap();
        archive.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn archive_comparison_ignores_timestamps_but_preserves_contents_and_modes() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        archive(&a, b"guest", 0o755, 1);
        archive(&b, b"guest", 0o755, 2);
        assert_ne!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
        assert!(archives_equal(&a, &b).unwrap());
        archive(&b, b"other", 0o755, 2);
        assert!(!archives_equal(&a, &b).unwrap());
        archive(&b, b"guest", 0o644, 2);
        assert!(!archives_equal(&a, &b).unwrap());
    }

    #[test]
    fn corrupt_gzip_trailers_and_hidden_suffixes_are_not_reused() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let valid = root.path().join("valid");
        let corrupt = root.path().join("corrupt");
        archive(&valid, b"guest", 0o755, 1);
        let original = fs::read(&valid).unwrap();
        let mut checksum = original.clone();
        let length = checksum.len();
        checksum[length - 8] ^= 1;
        let mut suffix = original.clone();
        suffix.extend_from_slice(b"hidden suffix");
        let mut hidden = GzEncoder::new(Vec::new(), Compression::fast());
        hidden.write_all(b"hidden second gzip member").unwrap();
        let mut concatenated = original.clone();
        concatenated.extend(hidden.finish().unwrap());
        for bytes in [
            original[..original.len() - 4].to_vec(),
            checksum,
            suffix,
            concatenated,
        ] {
            fs::write(&corrupt, bytes).unwrap();
            assert!(!archives_equal(&valid, &corrupt).unwrap_or(false));
        }
    }
}
