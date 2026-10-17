//! Host POSIX process tests. These do not establish either guest backend's
//! transport, application protocol, libc packaging, or native acceptance.
#![cfg(unix)]
use seedbox_test::process::Guarded;
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
        .guarded_output()
        .expect("C compiler from the project development shell");
    assert!(
        build.status.success(),
        "supervisor fixture compilation failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let result = Command::new(&executable)
        .guarded_output()
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
        .guarded_output()
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
        .guarded_output()
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
        .guarded_output()
        .expect("C compiler from project development shell");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let result = Command::new(executable)
        .guarded_output()
        .expect("credential fixture");
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        result.stdout,
        b"runtime credential sentinels rejected before mutation\n"
    );
}

// The guest network sources use Linux-only socket, interface and O_DIRECT APIs.
#[cfg(target_os = "linux")]
#[test]
fn network_argument_parsing_and_interface_configuration() {
    let directory = tempfile::tempdir().expect("isolated network test directory");
    let executable = directory.path().join("guest-network-test");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source.join("guest_network_test.c"))
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .expect("C compiler from project development shell");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let result = Command::new(executable)
        .guarded_output()
        .expect("network fixture");
    assert!(result.status.success(), "{result:?}");
    // Configuration needs an unprivileged user and network namespace; a host
    // without them reports a skip rather than a pass.
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        [
            "guest network parse passed; configure passed\n",
            "guest network parse passed; configure skipped\n"
        ]
        .contains(&&*stdout),
        "{stdout}"
    );
    if stdout.contains("skipped") {
        eprintln!("guest network configuration skipped: no unprivileged namespaces");
    }
}

/// Host loopback check of the shared guest network probe's own logic. Guest
/// ARP and simulated delivery are covered only by native machine tests.
// The guest network sources use Linux-only socket, interface and O_DIRECT APIs.
#[cfg(target_os = "linux")]
#[test]
fn network_probe_echoes_udp_and_tcp_over_loopback() {
    use std::io::{BufRead, BufReader};
    let directory = tempfile::tempdir().expect("isolated probe test directory");
    let executable = directory.path().join("network-probe");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/network-probe.c");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .expect("C compiler from project development shell");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut server = Command::new(&executable)
        .args(["server", "0"])
        .stdout(std::process::Stdio::piped())
        .guarded_spawn()
        .expect("probe server");
    let mut ready = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let port = ready
        .strip_prefix("NETPROBE ready port=")
        .and_then(|port| port.trim_end().parse::<u16>().ok())
        .unwrap_or_else(|| panic!("{ready:?}"));
    let result = Command::new(&executable)
        .args(["client", "127.0.0.1", &port.to_string(), "1048576"])
        .guarded_output()
        .expect("probe client");
    server.kill().unwrap();
    server.wait().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        String::from_utf8_lossy(&result.stdout),
        "NETPROBE udp ok attempts=1\nNETPROBE tcp ok bytes=1048576 connects=1\nNETPROBE done\n"
    );
    let usage = Command::new(&executable)
        .args(["client", "127.0.0.1", "0", "1"])
        .guarded_output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(64));
}

/// Host check of the probe's `idle` mode and argument validation. The
/// `server-disk` mode writes `/dev/vda` and runs only inside guests.
// The guest network sources use Linux-only socket, interface and O_DIRECT APIs.
#[cfg(target_os = "linux")]
#[test]
fn network_probe_idle_mode_and_usage() {
    use std::io::{BufRead, BufReader};
    let directory = tempfile::tempdir().expect("isolated probe test directory");
    let executable = directory.path().join("network-probe");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/network-probe.c");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .expect("C compiler from project development shell");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut idle = Command::new(&executable)
        .arg("idle")
        .stdout(std::process::Stdio::piped())
        .guarded_spawn()
        .expect("idle probe");
    let mut line = String::new();
    BufReader::new(idle.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "NETPROBE idle\n");
    assert!(idle.try_wait().unwrap().is_none(), "idle keeps running");
    idle.kill().unwrap();
    idle.wait().unwrap();
    for arguments in [
        &["idle", "extra"][..],
        &["server-disk"],
        &["server-disk", "65536"],
        &["server-disk", "x"],
    ] {
        let usage = Command::new(&executable)
            .args(arguments)
            .guarded_output()
            .unwrap();
        assert_eq!(usage.status.code(), Some(64), "{arguments:?}");
    }
}
