use seedbox_application::{
    execution::{ExecutionBackend, WitnessMode},
    store::ContentDigest,
    vmm,
};
use std::path::Path;

#[test]
fn explicit_combined_cli_is_retained_with_its_actual_version() {
    use seedbox_application::store::ContentStore;
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let binary = Path::new(env!("CARGO_BIN_EXE_seedbox"));
    let expected = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .unwrap();
    assert!(expected.status.success());
    let version = String::from_utf8(expected.stdout).unwrap();
    let executor = vmm::VmmExecutor::new_detected(binary, &store, 512, WitnessMode::Exits).unwrap();
    let profile = executor.profile();
    let descriptor = profile.semantic_parameters.engine.as_ref().unwrap();
    assert_eq!(
        descriptor.simulator_version,
        version.trim().strip_prefix("seedbox ").unwrap()
    );
    assert_eq!(
        descriptor.kind,
        seedbox_application::engine::ExecutableKind::InternalWorker
    );
    assert_eq!(
        profile.engine,
        ContentDigest::of(&std::fs::read(binary).unwrap())
    );
}
