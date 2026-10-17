//! Shared process-entry assertions: only EOF, malformed framing and preboot
//! requests. No Boot request is sent, so no hypervisor or guest is constructed.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: spawns here never race a Seedbox process writing an executable"
)]
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
            .spawn()
            .unwrap(),
    );
    child.0.stdin.take().unwrap().write_all(input).unwrap();
    let status = seedbox_test_time::HangGuard::new(
        "preboot worker exits after EOF or malformed request",
        Duration::from_secs(5),
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
    ] {
        let (status, stdout, stderr) = invoke(binary, &["--seedbox-pv-worker-v1".into()], bytes);
        assert_eq!(status.code(), Some(2));
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(error.starts_with(diagnostic), "{error}");
    }
}
pub fn preboot_requests(binary: &Path) {
    let mut input = Vec::new();
    for value in [
        serde_json::json!({"Capture":"unused"}),
        serde_json::json!({"ExportDisk":"unused"}),
        serde_json::json!({"Run":{"now":0,"upto":0,"syscall_cost_ns":1,"shutdown":false,"lifecycle":[],"frames":0}}),
    ] {
        let body = serde_json::to_vec(&value).unwrap();
        input.extend_from_slice(&(body.len() as u32).to_le_bytes());
        input.extend_from_slice(&body);
    }
    input.extend_from_slice(&[0, 0]); // Empty host-frame trailer, consumed before Run rejection.
    let (status, stdout, stderr) = invoke(binary, &["--seedbox-pv-worker-v1".into()], &input);
    assert_eq!(status.code(), Some(0));
    assert!(stderr.is_empty());
    let mut output = stdout.as_slice();
    for message in [
        "PV capture before boot",
        "PV disk export before boot",
        "PV run before boot",
    ] {
        let length = u32::from_le_bytes(output[..4].try_into().unwrap()) as usize;
        let response: serde_json::Value = serde_json::from_slice(&output[4..4 + length]).unwrap();
        assert_eq!(response, serde_json::json!({"Error":message}));
        output = &output[4 + length..];
    }
    assert!(output.is_empty());
}
