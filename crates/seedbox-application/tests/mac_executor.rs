//! Retained HVF executors compose with content storage without opening a VM.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use seedbox_application::{
    execution::{
        Backend, ExecutionBackend, ExecutionProfile, HvfParameters, SemanticParameters, WitnessMode,
    },
    mac::MacExecutor,
    store::{ContentDigest, ContentStore},
};

#[test]
fn retained_constructor_preserves_descriptive_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let engine = directory.path().join("engine");
    std::fs::write(&engine, b"new executable").unwrap();
    let store = ContentStore::new(directory.path().join("objects"));
    let previous = ExecutionProfile {
        backend: Backend::Hvf,
        engine: ContentDigest::of(b"previous executable"),
        witness_mode: WitnessMode::FullState,
        semantic_parameters: SemanticParameters {
            engine: Some(seedbox_application::engine::arm_descriptor()),
            hvf: Some(HvfParameters {
                boot_policy: "retained boot policy".into(),
                entropy_policy: "retained entropy policy".into(),
                device_policy: "retained device policy".into(),
                host_release: "retained host release".into(),
                cpu_features: ContentDigest::of(b"retained features").hex().into(),
                guest_kernel: ContentDigest::of(b"retained kernel"),
                launch_max_skew_ns: seedbox_application::engine::ARM_LAUNCH_MAX_SKEW_NS,
            }),
            ..SemanticParameters::new(512)
        },
    };
    let executor = MacExecutor::from_retained(&engine, &store, previous.clone()).unwrap();
    let retained = executor.profile();
    assert_eq!(retained.semantic_parameters, previous.semantic_parameters);
    assert_eq!(retained.engine, ContentDigest::of(b"new executable"));
    assert_eq!(store.read(&retained.engine).unwrap(), b"new executable");
}
