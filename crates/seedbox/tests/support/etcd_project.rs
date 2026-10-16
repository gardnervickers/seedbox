//! Acceptance inputs derived from the real example, never a replacement oracle.
use seedbox_api::{ComparisonStatus, SimulationComparison};
use seedbox_spec::project::Project;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const BASELINE_SEED: u64 = 123;
pub const EXTRA_SEEDS: [u64; 2] = [124, 125];
pub const WRITES_PER_WORKER: usize = 4;
pub const CONCURRENCY: usize = 1;

/// Explicit CLI inputs: seed is never inherited from the process environment.
/// Pause lets acceptance inspect the retained Driver definition before release.
pub fn submit_arguments(project: &Path, seed: u64) -> Vec<std::ffi::OsString> {
    vec![
        "run".into(),
        project.as_os_str().to_owned(),
        "--seed".into(),
        seed.to_string().into(),
        "--paused".into(),
        "--detach".into(),
    ]
}

pub fn example_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/etcd")
}

pub fn canonical_project() -> Project {
    serde_yaml_ng::from_str(&fs::read_to_string(example_directory().join("seedbox.yaml")).unwrap())
        .unwrap()
}

/// A deliberately small, explicit acceptance variant. The checked-in example
/// remains untouched; every member, disk, image and the actual Go checker stay
/// the same. Fixed writes still require every requested acknowledgement and
/// cross-member value/revision/HashKV agreement, not merely a live cluster.
pub fn acceptance_project() -> Project {
    let mut project = canonical_project();
    project.oracle.build.as_mut().unwrap().context =
        example_directory().join("workload").canonicalize().unwrap();
    let command = project.oracle.command.as_mut().unwrap();
    for (flag, value) in [
        ("--writes", WRITES_PER_WORKER.to_string()),
        ("--concurrency", CONCURRENCY.to_string()),
        ("--duration", "180s".to_owned()),
    ] {
        let index = command.iter().position(|arg| arg == flag).unwrap();
        command[index + 1] = value;
    }
    project
}

pub fn write_acceptance_project(directory: &Path) -> PathBuf {
    let path = directory.join("seedbox.yaml");
    fs::write(
        &path,
        serde_yaml_ng::to_string(&acceptance_project()).unwrap(),
    )
    .unwrap();
    path
}

/// A headline MATCH alone is insufficient, and missing evidence is not success.
pub fn require_match(comparison: &SimulationComparison) -> Result<(), String> {
    if comparison.status != ComparisonStatus::Match
        || !comparison.outcome_matches
        || comparison.witness_matches != Some(true)
        || comparison.full_state_matches != Some(true)
        || !comparison.fault_history_matches
    {
        return Err(format!("incomplete determinism comparison: {comparison:?}"));
    }
    Ok(())
}
