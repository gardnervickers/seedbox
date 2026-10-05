use std::{fs, path::PathBuf};

#[test]
#[ignore = "requires pinned x86-64 Redis fixtures; set SEEDBOX_REDIS_FIXTURES to their bin directory"]
fn real_redis_ttl_fresh_processes_host_load_and_negative_control() {
    let fixtures = PathBuf::from(
        std::env::var_os("SEEDBOX_REDIS_FIXTURES")
            .expect("set SEEDBOX_REDIS_FIXTURES to the pinned Redis bin directory"),
    );
    let world = tempfile::tempdir().unwrap();
    let root = world.path();
    fs::create_dir_all(root.join("build/redis")).unwrap();
    fs::create_dir_all(root.join("examples")).unwrap();
    for name in ["redis-server", "redis-ttl-client"] {
        fs::copy(fixtures.join(name), root.join("build/redis").join(name)).unwrap();
    }
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/redis.json"),
        root.join("examples/redis.json"),
    )
    .unwrap();
    let summary = seedbox::verification::verify_redis(
        root,
        &PathBuf::from(env!("CARGO_BIN_EXE_seedbox")),
        2,
        &root.join("artifacts/verification.json"),
    )
    .unwrap();
    assert_eq!(summary["incorrect_ttl_rejected"], true);
    assert_eq!(summary["fresh_process_reports_identical"], true);
}
