//! Executor retention and replacement through public application boundaries.
use seedbox_application::{
    execution::*,
    store::{ContentDigest, ContentStore},
    vmm::VmmExecutor,
};
#[path = "support/process.rs"]
mod process;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[test]
fn internal_world_refusal_precedes_retention_or_input_reading() {
    let temporary = tempfile::tempdir().unwrap();
    let store_path = temporary.path().join("objects");
    let store = ContentStore::new(&store_path);
    let missing = temporary.path().join("missing");
    let expected = "built-in Compose world execution requires Linux x86-64; ARM world integration is not available yet";
    let result = VmmExecutor::new_internal(&missing, &store, 512, WitnessMode::Exits);
    assert_eq!(result.unwrap_err().to_string(), expected);
    assert!(!store_path.exists());
    assert_eq!(
        VmmExecutor::doctor_internal(&missing, &missing)
            .unwrap_err()
            .to_string(),
        expected
    );
    assert!(!store_path.exists());
}

#[test]
fn replay_preserves_historical_policy_data_without_current_defaults() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("retained-worker");
    std::fs::write(&executable, b"retained executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let profile = VmmExecutor::new(&executable, &store, 512, WitnessMode::Exits)
        .unwrap()
        .profile();
    let before = serde_json::to_vec(&profile).unwrap();
    let executor = VmmExecutor::from_retained(&executable, &store, profile).unwrap();
    assert_eq!(serde_json::to_vec(&executor.profile()).unwrap(), before);
    seedbox_application::admission::validate_profile(&executor.profile()).unwrap();
    assert_eq!(
        seedbox_application::engine::recorded_launch(&executor.profile()).unwrap(),
        seedbox_application::engine::ExecutableKind::Standalone
    );
}

#[test]
fn explicit_override_uses_replacement_contract_in_both_directions() {
    if !process::isolated(concat!(
        module_path!(),
        "::explicit_override_uses_replacement_contract_in_both_directions"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let combined = directory.path().join("combined");
    let standalone = directory.path().join("standalone");
    std::fs::write(
        &combined,
        b"#!/bin/sh\nprintf 'seedbox 0.2.0-replacement\\n'\n",
    )
    .unwrap();
    std::fs::write(&standalone, b"#!/bin/sh\nexit 2\n").unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let original = VmmExecutor::new(&standalone, &store, 512, WitnessMode::Exits)
        .unwrap()
        .with_page_timer(true)
        .with_deferred_tx(true)
        .with_torn_writes(true);
    let combined =
        VmmExecutor::from_override(&combined, &store, original.profile().clone()).unwrap();
    assert_eq!(
        combined
            .profile()
            .semantic_parameters
            .engine
            .as_ref()
            .unwrap()
            .simulator_version,
        "0.2.0-replacement"
    );
    assert!(combined.profile().semantic_parameters.torn_writes);
    let standalone = VmmExecutor::from_override(&standalone, &store, combined.profile()).unwrap();
    assert_eq!(standalone.profile(), original.profile());
    // Explicit fresh runs perform the same detection; retained replay does not.
    let detected = VmmExecutor::new_detected(
        &directory.path().join("combined"),
        &store,
        512,
        WitnessMode::Exits,
    )
    .unwrap();
    assert_eq!(
        seedbox_application::engine::recorded_launch(&detected.profile()).unwrap(),
        seedbox_application::engine::ExecutableKind::InternalWorker
    );
}

#[test]
fn internal_profile_retains_identity_and_replays_through_worker_contract() {
    use seedbox_application::engine::{recorded_launch, ExecutableKind};
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("retained-worker");
    std::fs::write(&executable, b"combined executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let executor = VmmExecutor::new_internal(&executable, &store, 512, WitnessMode::Exits).unwrap();
    // A retained Linux profile is inspectable on a Mac without host admission.
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let executor = {
        let executor = VmmExecutor::new(&executable, &store, 512, WitnessMode::Exits).unwrap();
        let mut profile = executor.profile();
        profile.semantic_parameters.engine = Some(seedbox_application::engine::kvm_descriptor(
            seedbox_application::engine::ExecutableKind::InternalWorker,
            seedbox_application::engine::SIMULATOR_VERSION.into(),
        ));
        VmmExecutor::from_retained(&executable, &store, profile).unwrap()
    };
    assert_eq!(
        recorded_launch(&executor.profile()).unwrap(),
        ExecutableKind::InternalWorker
    );
    let bytes = serde_json::to_vec(&executor.profile()).unwrap();
    let retained: ExecutionProfile = serde_json::from_slice(&bytes).unwrap();
    let replay = VmmExecutor::from_retained(&executable, &store, retained).unwrap();
    assert_eq!(serde_json::to_vec(&replay.profile()).unwrap(), bytes);
    // Historical standalone profiles keep their launch contract.
    let legacy = VmmExecutor::new(&executable, &store, 512, WitnessMode::Exits).unwrap();
    assert_eq!(
        recorded_launch(&legacy.profile()).unwrap(),
        ExecutableKind::Standalone
    );
}

#[test]
fn recorded_descriptor_is_data_and_unknown_settings_fail() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("old-worker");
    std::fs::write(&executable, b"retained worker").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let mut profile = VmmExecutor::new(&executable, &store, 512, WitnessMode::Exits)
        .unwrap()
        .profile();
    profile.semantic_parameters.engine = Some(seedbox_application::engine::kvm_descriptor(
        seedbox_application::engine::ExecutableKind::InternalWorker,
        "0.1.0-dev.1".into(),
    ));
    let original = serde_json::to_vec(&profile).unwrap();
    let executor = VmmExecutor::from_retained(&executable, &store, profile.clone()).unwrap();
    assert_eq!(serde_json::to_vec(&executor.profile()).unwrap(), original);
    assert_eq!(
        seedbox_application::engine::recorded_launch(&executor.profile()).unwrap(),
        seedbox_application::engine::ExecutableKind::InternalWorker
    );
    let mut json = serde_json::to_value(&profile).unwrap();
    json["semantic_parameters"]["timebase"] = "counted-branches-v1".into();
    assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
}

#[test]
fn replay_override_changes_only_retained_executable_digest() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("replacement-worker");
    std::fs::write(&executable, b"replacement executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let profile = ExecutionProfile {
        backend: Backend::Kvm,
        engine: ContentDigest::of(b"original executable"),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: SemanticParameters::new(512),
    };
    let mut expected = profile.clone();
    expected.engine = ContentDigest::of(b"replacement executable");
    let executor = VmmExecutor::from_retained(&executable, &store, profile).unwrap();
    assert_eq!(executor.profile(), expected);
}
