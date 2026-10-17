use seedbox_test::process::Guarded;
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
        .guarded_status()
        .unwrap()
        .success());
    assert!(Command::new(binary).guarded_status().unwrap().success());
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
        .guarded_status()
        .unwrap()
        .success());
    assert!(Command::new(binary).guarded_status().unwrap().success());
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
        .guarded_status()
        .unwrap()
        .success());
    assert!(Command::new(binary).guarded_status().unwrap().success());
}

#[test]
fn guest_application_output_preserves_stream_identity_and_every_byte() {
    use seedbox_engine_protocol::application_log::{decode, Stream};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest");
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("application-output-test");
    assert!(Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(root.join("application-output-test.c"))
        .arg("-o")
        .arg(&binary)
        .guarded_status()
        .unwrap()
        .success());
    let output = Command::new(binary).guarded_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let chunks: Vec<_> = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| decode(line).unwrap().unwrap())
        .collect();
    assert_eq!(chunks.len(), 2);
    for (chunk, stream) in chunks.iter().zip([Stream::Stdout, Stream::Stderr]) {
        assert_eq!(chunk.stream, stream);
        assert_eq!(chunk.bytes, (0..=255).collect::<Vec<u8>>());
    }
}
