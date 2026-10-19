//! Fast preparation-smoke inputs, never canonical etcd acceptance evidence.
//! bd-207 must run the checked-in examples/etcd unchanged.
use seedbox_api::{
    ComparisonStatus, ExecutionTermination, Phase, SimulationComparison, SimulationStatus,
    TestVerdict,
};
use seedbox_spec::project::Project;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Pv,
    Tcg,
}
impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pv => "pv",
            Self::Tcg => "tcg",
        }
    }
    pub fn profile(self) -> &'static str {
        match self {
            Self::Pv => "driver-pv",
            Self::Tcg => "driver-tcg",
        }
    }
}

pub const BASELINE_SEED: u64 = 123;
pub const WRITES_PER_WORKER: usize = 4;
pub const CONCURRENCY: usize = 1;

/// PV uses the existing default; TCG selects the public software backend.
/// Seed is explicit, and pause permits inspecting retained inputs before release.
pub fn submit_arguments(
    project: &Path,
    seed: u64,
    backend: Backend,
) -> Result<Vec<std::ffi::OsString>, String> {
    let mut arguments = vec![
        "run".into(),
        project.as_os_str().to_owned(),
        "--seed".into(),
        seed.to_string().into(),
        "--paused".into(),
        "--detach".into(),
    ];
    if backend == Backend::Tcg {
        arguments.extend(["--backend".into(), "tcg".into()]);
    }
    Ok(arguments)
}

pub fn example_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/etcd")
}

pub fn canonical_project() -> Project {
    serde_yaml_ng::from_str(&fs::read_to_string(example_directory().join("seedbox.yaml")).unwrap())
        .unwrap()
}

/// A deliberately small preparation/fast-test variant, NOT bd-207 acceptance.
/// Canonical acceptance must run examples/etcd unchanged. The checked-in example
/// remains byte-identical; every member, disk, image and the actual Go checker stay
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

pub fn require_oracle_output(output: &[u8]) -> Result<(), String> {
    let output = std::str::from_utf8(output).map_err(|e| e.to_string())?;
    let prefix = format!("RESULT pass acked={} ", WRITES_PER_WORKER * CONCURRENCY);
    if !output
        .lines()
        .any(|line| line.starts_with(&prefix) && line.contains(" rev=") && line.contains(" hash="))
    {
        return Err(format!(
            "missing real Go checker acknowledgement/revision/hash result: {output}"
        ));
    }
    Ok(())
}

/// Identify the selected Driver backend and the presence of its opaque retained
/// closure before release. The runtime validates that closure's contents; this
/// harness never constructs or substitutes it.
pub fn require_backend(execution: &serde_json::Value, backend: Backend) -> Result<(), String> {
    if execution.get("backend").and_then(serde_json::Value::as_str) != Some(backend.profile())
        || !execution
            .get("driver_world")
            .is_some_and(serde_json::Value::is_object)
    {
        return Err(format!(
            "expected retained {} Driver closure, got {execution}",
            backend.name()
        ));
    }
    Ok(())
}

/// Equal failures are not acceptance. The designated real oracle must exit zero
/// and normal execution must finish cleanly before comparing retained evidence.
pub fn require_pass(status: &SimulationStatus) -> Result<(), String> {
    if status.phase != Phase::Completed
        || status.verdict() != TestVerdict::Passed
        || status.termination != Some(ExecutionTermination::OracleExited)
    {
        return Err(format!(
            "etcd acceptance did not complete cleanly with oracle PASS: {status:?}"
        ));
    }
    Ok(())
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
