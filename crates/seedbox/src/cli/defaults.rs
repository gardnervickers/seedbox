//! Portable CLI path resolution; complete builds keep guest inputs beside the executable.
use seedbox_application::{execution::Engine, Error, Result};
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

/// The running executable is the engine of every run and replay. Its source
/// revision is recorded as provenance only.
pub fn engine() -> Result<Engine> {
    let executable = env::current_exe().map_err(|error| {
        Error::Setup(format!(
            "cannot locate the seedbox executable: {error}; reinstall seedbox"
        ))
    })?;
    Ok(Engine {
        executable,
        revision: build_revision(),
    })
}

/// Builds with uncommitted changes still run; say that their recorded
/// revision does not identify the exact source.
pub fn warn_dirty_build() {
    if let Some(revision) = build_revision().filter(|r| r.ends_with("-dirty")) {
        eprintln!(
            "warning: seedbox was built with uncommitted changes ({revision}); runs record this revision, which does not identify their exact source"
        );
    }
}

/// The Git revision this executable was built from, `-dirty` with uncommitted
/// changes; `None` when the build had no Git metadata (build-runtime.rs).
pub fn build_revision() -> Option<String> {
    let revision = env!("SEEDBOX_BUILD_REVISION");
    (!revision.is_empty()).then(|| revision.to_owned())
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
        assert_eq!(
            existing("--kernel", "/nonexistent/kernel".into())
                .unwrap_err()
                .to_string(),
            "--kernel /nonexistent/kernel: not found"
        );
    }
    #[test]
    fn engine_is_the_current_executable_and_its_build() {
        let engine = engine().unwrap();
        assert_eq!(engine.executable, env::current_exe().unwrap());
        assert_eq!(engine.revision, build_revision());
    }
}
