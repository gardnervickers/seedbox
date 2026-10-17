#![allow(dead_code)]

use seedbox_test_time::HangGuard;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// File-backed capture avoids a full stdout pipe deadlocking the timeout loop.
/// All process state is child-local; no environment or cwd is changed globally.
pub fn run(command: &mut Command, seconds: u64) -> Output {
    let capture = tempfile::tempdir().unwrap();
    let stdout = capture.path().join("stdout");
    let stderr = capture.path().join("stderr");
    command
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = OwnedChild(
        seedbox_test::process::spawn(command).unwrap_or_else(|e| panic!("{command:?}: {e}")),
    );
    let guard = HangGuard::new("xtask fixture process exit", Duration::from_secs(seconds));
    let status = guard
        .wait_for(|| child.0.try_wait().transpose())
        .unwrap_or_else(|error| {
            panic!(
                "{command:?} exceeded {seconds}s: {error}\nstdout: {}\nstderr: {}",
                fs::read_to_string(&stdout).unwrap_or_default(),
                fs::read_to_string(&stderr).unwrap_or_default()
            )
        })
        .unwrap();
    Output {
        status,
        stdout: fs::read(stdout).unwrap(),
        stderr: fs::read(stderr).unwrap(),
    }
}

pub fn success(command: &mut Command, seconds: u64) -> Output {
    let output = run(command, seconds);
    assert!(
        output.status.success(),
        "{command:?}: {}",
        diagnostic(&output)
    );
    output
}

pub fn diagnostic(output: &Output) -> String {
    format!(
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn python(tool: &str) -> Command {
    let mut command = Command::new("python3");
    command.arg(root().join("tools/runtime").join(tool));
    command
}

// The notice build helper exposes a function, not a CLI. This is only an
// invocation bridge: archive generation, cases and all assertions are Rust.
pub fn notices(sources: &Path, component: Option<&str>) -> Output {
    let mut command = Command::new("python3");
    command
        .arg("-c")
        .arg("import pathlib, runpy, sys; m = runpy.run_path(sys.argv[1]); sys.stdout.buffer.write(m['notices'](pathlib.Path(sys.argv[2]), (sys.argv[3],)) if len(sys.argv) > 3 else m['notices'](pathlib.Path(sys.argv[2])))")
        .arg(root().join("tools/runtime/notices.py"))
        .arg(sources);
    if let Some(component) = component {
        command.arg(component);
    }
    run(&mut command, 30)
}

pub fn notice_components() -> Vec<String> {
    let output = success(
        Command::new("python3")
            .arg("-c")
            .arg("import runpy, sys; print('\\n'.join(runpy.run_path(sys.argv[1])['NOTICES']))")
            .arg(root().join("tools/runtime/notices.py")),
        30,
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[derive(Debug)]
pub struct Member {
    pub name: String,
    pub mode: u64,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub bytes: Vec<u8>,
}

/// Read and validate tar headers/payloads without extracting guest paths.
/// Empty archives cannot be valid helper packages, even when their tar framing
/// is valid; reject them rather than allowing metadata assertions to be vacuous.
pub fn archive_members(bytes: &[u8]) -> std::io::Result<Vec<Member>> {
    use std::io::{Error, ErrorKind, Read};
    let mut archive = tar::Archive::new(bytes);
    let mut members = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .into_owned()
            .into_os_string()
            .into_string()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "non-UTF-8 package member"))?;
        let header = entry.header();
        let (mode, uid, gid, mtime) = (
            u64::from(header.mode()?),
            header.uid()?,
            header.gid()?,
            header.mtime()?,
        );
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        members.push(Member {
            name,
            mode,
            uid,
            gid,
            mtime,
            bytes,
        });
    }
    if members.is_empty() {
        return Err(Error::new(ErrorKind::InvalidData, "empty helper package"));
    }
    Ok(members)
}

pub fn package_members(path: &Path) -> Vec<Member> {
    let output = success(Command::new("gzip").arg("-dc").arg(path), 30);
    archive_members(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid helper package {}: {error}", path.display()))
}
