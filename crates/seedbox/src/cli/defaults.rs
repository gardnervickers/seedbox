//! Portable CLI path resolution; complete builds keep guest inputs beside the executable.
use seedbox::{Error, Result};
use std::{
    env,
    path::{Path, PathBuf},
};
const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// A Compose file path, or the Compose file inside a project directory.
pub fn compose_file(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return existing("Compose file", path.to_owned());
    }
    COMPOSE_FILES
        .iter()
        .map(|name| path.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            let searched: Vec<_> = COMPOSE_FILES.iter().map(|name| path.join(name)).collect();
            missing("Compose file", &searched, "pass the file path directly")
        })
}

/// The VMM is linked into this executable. An explicit path selects the legacy
/// standalone worker contract for development and historical runs.
pub fn engine(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return existing("--vmm", path);
    }
    env::current_exe().map_err(|error| {
        Error::Setup(format!(
            "cannot locate the built-in VMM executable: {error}; reinstall seedbox or pass --vmm"
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
        assert_eq!(engine(Some(file.path().into())).unwrap(), file.path());
        assert_eq!(
            existing("--kernel", "/nonexistent/kernel".into())
                .unwrap_err()
                .to_string(),
            "--kernel /nonexistent/kernel: not found"
        );
    }
    #[test]
    fn engine_defaults_to_current_executable() {
        assert_eq!(engine(None).unwrap(), env::current_exe().unwrap());
    }
}
