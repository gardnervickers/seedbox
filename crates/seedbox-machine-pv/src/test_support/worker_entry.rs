//! Shared process-entry assertions: only EOF, malformed framing and preboot
//! requests. No Boot request is sent, so no hypervisor or guest is constructed.
use seedbox_test::process::Guarded;
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub fn invoke(
    binary: &Path,
    arguments: &[OsString],
    input: &[u8],
) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("home");
    fs::create_dir(&home).unwrap();
    let state = scratch.path().join("state");
    let stdout = scratch.path().join("stdout");
    let stderr = scratch.path().join("stderr");
    let mut child = Owned(
        Command::new(binary)
            .args(arguments)
            .current_dir(&home)
            .env_clear()
            .env("HOME", &home)
            .env("SEEDBOX_STATE", &state)
            .env("PATH", "")
            .stdin(Stdio::piped())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .guarded_spawn()
            .unwrap(),
    );
    child.0.stdin.take().unwrap().write_all(input).unwrap();
    let status = seedbox_test_time::HangGuard::new(
        "preboot worker exits after EOF or malformed request",
        Duration::from_secs(30),
    )
    .wait_for(|| child.0.try_wait().unwrap())
    .expect("preboot worker did not exit");
    assert!(
        !state.exists(),
        "private worker must not initialize CLI state"
    );
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    (status, fs::read(stdout).unwrap(), fs::read(stderr).unwrap())
}
pub fn eof_and_trailing_arguments(binary: &Path) {
    let mut variants = vec![
        vec!["--seedbox-pv-worker-v1".into()],
        vec![
            "--seedbox-pv-worker-v1".into(),
            "--help".into(),
            "ignored".into(),
        ],
    ];
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        variants.push(vec![
            "--seedbox-pv-worker-v1".into(),
            OsString::from_vec(vec![0xff]),
        ]);
    }
    for arguments in variants {
        let (status, stdout, stderr) = invoke(binary, &arguments, b"");
        assert_eq!(status.code(), Some(0));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }
}
pub fn malformed_streams(binary: &Path) {
    for (bytes, diagnostic) in [
        (&b"\x04"[..], "PV protocol header"),
        (&b"\x04\0\0\0{}"[..], "PV protocol body"),
        (&b"\x04\0\0\0null"[..], "PV protocol decode"),
        (&b"\xff\xff\xff\xff"[..], "PV protocol message too large"),
        (
            &b"\x0e\0\0\0{\"Capture\":\"\"}"[..],
            "PV request before transport attach",
        ),
    ] {
        let (status, stdout, stderr) = invoke(binary, &["--seedbox-pv-worker-v1".into()], bytes);
        assert_eq!(status.code(), Some(2));
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(error.starts_with(diagnostic), "{error}");
    }
}
/// The client's own spawn attaches its shared region, and the worker answers
/// requests that need a boot through it, frames trailer included.
#[cfg(seedbox_pv)]
pub fn preboot_requests(binary: &Path) {
    let binary = binary.to_owned();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(crate::machine::pv::preboot_errors(&binary)));
    let errors = seedbox_test_time::HangGuard::new(
        "attached worker answers preboot requests",
        Duration::from_secs(30),
    )
    .recv(&receiver)
    .expect("attached worker did not answer")
    .unwrap();
    assert_eq!(
        errors,
        [
            Some("PV capture before boot".into()),
            Some("PV disk export before boot".into()),
            Some("PV run before boot".into()),
        ]
    );
}
/// The worker's end of the client's shared region, for synthetic workers:
/// answers `Attach { capacity }` on `wake` and returns the attached stream.
#[cfg(seedbox_pv)]
pub fn accept_transport<R: std::io::Read, W: Write>(
    capacity: u64,
    sleep: R,
    wake: W,
) -> Result<impl std::io::Read + Write, String> {
    crate::machine::pv::accept(capacity, sleep, wake)
}
