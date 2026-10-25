//! Portable CLI path resolution and computed execution defaults; complete builds
//! keep guest inputs beside the executable.
use seedbox_application::{Error, Result};
use std::{
    env,
    path::{Path, PathBuf},
};
const PROJECT_FILES: [&str; 2] = ["seedbox.yaml", "seedbox.yml"];

/// Campaign plan defaults: 32 members in generations of 8, a quarter of each
/// later generation fresh roots, and 8 members kept as branch sources.
pub const CAMPAIGN_BUDGET: u32 = 32;
pub const CAMPAIGN_BATCH: u32 = 8;
pub const CAMPAIGN_FRESH_PERMILLE: u32 = 250;
pub const CAMPAIGN_CORPUS: u32 = 8;
/// Analysis defaults: 8 runs per arm and 4 time arms.
pub const ANALYSIS_SAMPLES: u32 = 8;
pub const ANALYSIS_TIMES: u32 = 4;

/// Resolve an explicit native project file or search the project directory.
pub fn project_file(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return existing("project file", path.to_owned());
    }
    PROJECT_FILES
        .iter()
        .map(|name| path.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            let searched: Vec<_> = PROJECT_FILES.iter().map(|name| path.join(name)).collect();
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

/// Machine worker threads when `--machine-workers` is omitted: one per machine,
/// with host CPUs shared across the server's execution slots, within 1..=64.
pub fn machine_workers(machines: usize, slots: usize) -> u32 {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    workers_for(machines, cpus, slots)
}

fn workers_for(machines: usize, cpus: usize, slots: usize) -> u32 {
    let share = cpus / slots.max(1);
    // Bounded by the clamp, so the conversion cannot truncate.
    machines.min(share).clamp(1, 64) as u32
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
    fn machine_workers_share_cpus_across_slots_within_bounds() {
        assert_eq!(workers_for(3, 16, 1), 3);
        assert_eq!(workers_for(8, 16, 4), 4);
        assert_eq!(workers_for(8, 2, 4), 1);
        assert_eq!(workers_for(0, 16, 1), 1);
        assert_eq!(workers_for(100, 256, 1), 64);
        assert_eq!(workers_for(3, 16, 0), 3);
    }

    #[test]
    fn project_directory_resolves_only_native_files() {
        let directory = tempfile::tempdir().unwrap();
        assert!(project_file(directory.path()).is_err());
        std::fs::write(directory.path().join("seedbox.yml"), "").unwrap();
        assert_eq!(
            project_file(directory.path()).unwrap(),
            directory.path().join("seedbox.yml")
        );
        std::fs::write(directory.path().join("seedbox.yaml"), "").unwrap();
        assert_eq!(
            project_file(directory.path()).unwrap(),
            directory.path().join("seedbox.yaml")
        );
        let file = directory.path().join("other.yaml");
        assert!(project_file(&file).is_err());
        std::fs::write(&file, "").unwrap();
        assert_eq!(project_file(&file).unwrap(), file);
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
}
