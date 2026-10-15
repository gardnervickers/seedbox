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

#[test]
fn guest_out_of_memory_frames_decode_as_the_kernel_reported_each_kill() {
    use seedbox_engine_protocol::out_of_memory::{decode, Process};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest");
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("oom-report-test");
    assert!(Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(root.join("oom-report-test.c"))
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap()
        .success());
    let output = Command::new(binary).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let processes: Vec<_> = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| decode(line).unwrap().unwrap().process.unwrap())
        .collect();
    let process = |pid, command: &str, sizes: [u64; 4]| Process {
        pid,
        command: command.into(),
        total_vm_kib: sizes[0],
        anon_rss_kib: sizes[1],
        file_rss_kib: sizes[2],
        shmem_rss_kib: sizes[3],
    };
    assert_eq!(
        processes,
        vec![
            process(412, "nats-server", [1_203_328, 446_228, 1024, 0]),
            process(7, "a (b) c", [u64::MAX, 2, 3, 4]),
            process(u32::MAX, &"\\x0a".repeat(15), [1, 1, 1, 1]),
        ]
    );
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
        .status()
        .unwrap()
        .success());
    let output = Command::new(binary).output().unwrap();
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
