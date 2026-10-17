//! Portable CLI path resolution; complete builds keep guest inputs beside the executable.
use seedbox_application::{Error, Result};
use std::{
    env,
    path::{Path, PathBuf},
};
const COMPOSE_FILES: [&str; 6] = [
    "seedbox.yaml",
    "seedbox.yml",
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// A native project file, falling back to an existing Compose project.
pub fn compose_file(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return existing("project file", path.to_owned());
    }
    COMPOSE_FILES
        .iter()
        .map(|name| path.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            let searched: Vec<_> = COMPOSE_FILES.iter().map(|name| path.join(name)).collect();
            missing("project file", &searched, "pass the file path directly")
        })
}

/// The worker is linked into this executable; runtime commands never build it.
pub fn engine() -> Result<PathBuf> {
    env::current_exe().map_err(|error| {
        Error::Setup(format!(
            "cannot locate the built-in worker executable: {error}; reinstall seedbox"
        ))
    })
}

/// A recorded run: a directory holding `manifest.json`.
pub fn run_directory(path: PathBuf) -> Result<PathBuf> {
    let path = existing("run directory", path)?;
    if path.join("manifest.json").is_file() {
        Ok(path)
    } else {
        Err(Error::Setup(format!(
            "{}: not a run directory (no manifest.json)",
            path.display()
        )))
    }
}

/// An explicitly named input, checked before any slow preparation starts.
pub fn existing(what: &str, path: PathBuf) -> Result<PathBuf> {
    if path.exists() {
        Ok(path)
    } else {
        Err(Error::Setup(format!(
            "{what} {}: not found",
            path.display()
        )))
    }
}

/// The caller decodes the selected archive; selection itself has no build or
/// environment dependency, so bundled and unbundled cases can always be tested.
// TODO: Re-evaluate: keep pure lookup here rather than making missing-input
// coverage depend on whether the CLI build embedded its runtime helpers.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HelperArchive<'a> {
    Embedded(&'a [u8]),
    File(PathBuf),
}
pub(super) fn helper_archive<'a>(
    explicit: Option<PathBuf>,
    embedded: Option<&'a [u8]>,
    beside: PathBuf,
) -> Result<HelperArchive<'a>> {
    if let Some(path) = explicit {
        return local_asset("runtime assets", path).map(HelperArchive::File);
    }
    if let Some(bytes) = embedded {
        return Ok(HelperArchive::Embedded(bytes));
    }
    local_asset("runtime assets", beside).map(HelperArchive::File)
}

pub(super) fn local_asset(flag: &str, path: PathBuf) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::Setup(format!("{flag} {}: not found; from the Seedbox checkout run `cargo xtask build-assets` and `cargo build --release`", path.display())))
    }
}

fn missing(what: &str, searched: &[PathBuf], fix: &str) -> Error {
    let searched: Vec<_> = searched
        .iter()
        .map(|path| format!("  {}", path.display()))
        .collect();
    Error::Setup(format!(
        "no {what} found; searched:\n{}\n{fix}",
        searched.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compose_directory_resolves_to_its_compose_file() {
        let directory = tempfile::tempdir().unwrap();
        assert!(compose_file(directory.path()).is_err());
        std::fs::write(directory.path().join("docker-compose.yml"), "").unwrap();
        assert_eq!(
            compose_file(directory.path()).unwrap(),
            directory.path().join("docker-compose.yml")
        );
        std::fs::write(directory.path().join("compose.yaml"), "").unwrap();
        assert_eq!(
            compose_file(directory.path()).unwrap(),
            directory.path().join("compose.yaml")
        );
        let file = directory.path().join("other.yaml");
        assert!(compose_file(&file).is_err());
        std::fs::write(&file, "").unwrap();
        assert_eq!(compose_file(&file).unwrap(), file);
    }

    #[test]
    fn explicit_inputs_are_checked_without_search() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            existing("project file", file.path().into()).unwrap(),
            file.path()
        );
        assert_eq!(
            existing("project file", "/nonexistent/seedbox.yaml".into())
                .unwrap_err()
                .to_string(),
            "project file /nonexistent/seedbox.yaml: not found"
        );
    }
    #[test]
    fn engine_defaults_to_current_executable() {
        assert_eq!(engine().unwrap(), env::current_exe().unwrap());
    }
    #[test]
    fn helper_archive_resolution_covers_bundled_and_unbundled_without_build_state() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing/runtime.tar.gz");
        let expected = format!("runtime assets {}: not found; from the Seedbox checkout run `cargo xtask build-assets` and `cargo build --release`", missing.display());
        assert_eq!(
            helper_archive(None, None, missing.clone())
                .unwrap_err()
                .to_string(),
            expected
        );
        let bytes = b"embedded lookup sentinel";
        assert_eq!(
            helper_archive(None, Some(bytes), missing.clone()).unwrap(),
            HelperArchive::Embedded(bytes)
        );
        let file = directory.path().join("runtime.tar.gz");
        std::fs::write(&file, b"local lookup sentinel").unwrap();
        assert_eq!(
            helper_archive(None, None, file.clone()).unwrap(),
            HelperArchive::File(file.clone())
        );
        for embedded in [None, Some(bytes.as_slice())] {
            assert_eq!(
                helper_archive(Some(file.clone()), embedded, missing.clone()).unwrap(),
                HelperArchive::File(file.clone())
            );
            assert_eq!(
                helper_archive(Some(missing.clone()), embedded, file.clone())
                    .unwrap_err()
                    .to_string(),
                expected
            );
        }
    }
}
