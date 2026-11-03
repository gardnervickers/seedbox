//! Public CLI transfer behavior against a stalled server.
use seedbox_test::process::Guarded;
use std::{fs, path::Path, process::Command, time::Duration};

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_seedbox"));
    command
        .env_clear()
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("PATH", "")
        .env("SEEDBOX_STATE", root.join("server"))
        .current_dir(root);
    command
}

#[test]
fn diagnostics_metadata_stall_exits_promptly_on_sigint_without_stdout() {
    use std::{
        io::{BufRead, Read},
        os::unix::net::UnixListener,
        sync::mpsc,
    };
    let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.path().join("server"))
        .unwrap();
    let listener = UnixListener::bind(root.path().join("server/server.sock")).unwrap();
    let (requested, ready) = mpsc::channel();
    let (release, done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        loop {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                continue;
            } // Daemon listening probe.
            assert!(line.starts_with("GET /v1/simulations/k7m2p9qx/diagnostics "));
            requested.send(()).unwrap();
            done.recv_timeout(Duration::from_secs(60)).unwrap(); // Intentionally never return metadata.
            break;
        }
    });
    let mut child = command(root.path())
        .args(["show", "k7m2p9qx", "--view", "diagnostics", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .guarded_spawn()
        .unwrap();
    ready.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    // Wait for the exit itself; the bound only stops a hung test.
    let pid = child.id() as i32;
    let mut child_stdout = child.stdout.take().unwrap();
    let mut child_stderr = child.stderr.take().unwrap();
    let (exited, exit) = mpsc::channel();
    let waiter = std::thread::spawn(move || exited.send(child.wait().unwrap()).unwrap());
    let status = match exit.recv_timeout(Duration::from_secs(30)) {
        Ok(status) => status,
        Err(_) => {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            waiter.join().unwrap();
            release.send(()).unwrap();
            thread.join().unwrap();
            panic!("diagnostics ignored SIGINT while metadata was stalled");
        }
    };
    waiter.join().unwrap();
    let mut stdout = Vec::new();
    let mut stderr = String::new();
    child_stdout.read_to_end(&mut stdout).unwrap();
    child_stderr.read_to_string(&mut stderr).unwrap();
    release.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(status.code(), Some(2));
    assert!(
        stdout.is_empty(),
        "cancelled metadata must not publish partial stdout"
    );
    assert!(stderr.starts_with("error: transfer cancelled"), "{stderr}");
}
