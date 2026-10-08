use std::process::Command;
#[cfg(target_os = "linux")]
#[test]
fn supervisor_io_survives_hidden_device_nodes_without_leaking_through_exec() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest");
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("io-test");
    assert!(Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(root.join("io-test.c"))
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap()
        .success());
    assert!(Command::new(binary).status().unwrap().success());
}
#[test]
fn workload_environment_defaults_gomaxprocs_without_overwriting_explicit_values() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest");
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("environment-test");
    assert!(Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(root.join("environment-test.c"))
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap()
        .success());
    assert!(Command::new(binary).status().unwrap().success());
}

#[test]
fn actual_guest_command_parser_rejects_ambiguous_and_uncorrelated_ids() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest");
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("control-test");
    assert!(Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(root.join("control-test.c"))
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap()
        .success());
    assert!(Command::new(binary).status().unwrap().success());
}
#[cfg(target_os = "linux")]
#[test]
fn actual_supervisor_reaps_before_ack_and_checks_exec_success() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest");
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("supervisor-test");
    assert!(Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Wno-unused-function"])
        .arg(root.join("supervisor-test.c"))
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap()
        .success());
    assert!(Command::new(binary).status().unwrap().success());
}
