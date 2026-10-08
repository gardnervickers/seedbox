//! Manifest-declared, checksum-verified offline evidence.
use crate::{orchestration::RunRecord, Error, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek},
    path::{Component, Path},
};
fn bad(command: &str, path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Setup(format!(
        "{command} {}: {error}; use an intact retained run directory (import bundles first)",
        path.display()
    ))
}
/// Open only manifest-declared evidence, checking its digest before interpreting
/// it. Omitted/unregistered files are not evidence, even if still on disk.
pub(crate) fn log(
    directory: &Path,
    record: &RunRecord,
    name: &str,
    command: &str,
) -> Result<Option<File>> {
    let Some(digest) = record.logs.get(name) else {
        return Ok(None);
    };
    let relative = Path::new(name);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(bad(command, directory, "unsafe evidence path"));
    }
    let mut path = directory.join("output");
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            path.push(component);
        }
        let meta = path
            .symlink_metadata()
            .map_err(|e| bad(command, &path, e))?;
        if meta.file_type().is_symlink() || (!meta.is_file() && !meta.is_dir()) {
            return Err(bad(
                command,
                &path,
                "evidence must be a regular file under real directories",
            ));
        }
    }
    let mut file = File::open(&path).map_err(|e| bad(command, &path, e))?;
    if !file
        .metadata()
        .map_err(|e| bad(command, &path, e))?
        .is_file()
    {
        return Err(bad(command, &path, "evidence must be a regular file"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| bad(command, &path, e))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if format!("{:x}", hash.finalize()) != digest.hex() {
        return Err(bad(command, &path, "evidence checksum mismatch"));
    }
    file.rewind().map_err(|e| bad(command, &path, e))?;
    Ok(Some(file))
}
