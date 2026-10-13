//! Host POSIX process tests. These do not establish either guest backend's
//! transport, application protocol, libc packaging, or native acceptance.
use std::{path::Path, process::Command};

#[test]
fn real_process_lifecycle_and_exact_output_bytes() {
    let directory = tempfile::tempdir().expect("isolated supervisor test directory");
    let executable = directory.path().join("guest-supervisor-test");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source.join("guest_supervisor.c"))
        .arg(source.join("guest_service.c"))
        .arg(source.join("guest_oracle.c"))
        .arg(source.join("guest_supervisor_test.c"))
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("C compiler from the project development shell");
    assert!(
        build.status.success(),
        "supervisor fixture compilation failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let result = Command::new(&executable)
        .output()
        .expect("run isolated POSIX supervisor fixture");
    assert!(
        result.status.success(),
        "supervisor fixture failed ({:?}): {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"guest supervisor host POSIX tests passed\n");
}

#[test]
fn real_oracle_socket_correlation_and_management_demultiplexing() {
    let directory = tempfile::tempdir().expect("isolated oracle test directory");
    let executable = directory.path().join("guest-oracle-test");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source.join("guest_supervisor.c"))
        .arg(source.join("guest_service.c"))
        .arg(source.join("guest_oracle.c"))
        .arg(source.join("guest_oracle_test.c"))
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("C compiler from project development shell");
    assert!(
        build.status.success(),
        "oracle fixture compile: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    // macOS Unix socket paths have a short fixed limit; the default macOS TMPDIR
    // adds a long randomized prefix. Keep fixture sockets below /private/tmp.
    let sockets = tempfile::Builder::new()
        .prefix("sbx-rpc-")
        .tempdir_in("/private/tmp")
        .or_else(|_| tempfile::tempdir())
        .expect("isolated short socket directory");
    let result = Command::new(executable)
        .arg(sockets.path())
        .output()
        .expect("run oracle socket fixture");
    assert!(
        result.status.success(),
        "oracle fixture failed ({:?}): {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"guest oracle host socket tests passed\n");
}

#[test]
fn runtime_credentials_reject_numeric_and_named_sentinels_before_mutation() {
    let directory = tempfile::tempdir().expect("isolated credential test directory");
    let executable = directory.path().join("guest-runtime-credentials-test");
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv/guest_runtime_credentials_test.c");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("C compiler from project development shell");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let result = Command::new(executable)
        .output()
        .expect("credential fixture");
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        result.stdout,
        b"runtime credential sentinels rejected before mutation\n"
    );
}
