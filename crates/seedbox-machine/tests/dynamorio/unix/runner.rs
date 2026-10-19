use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{
    fs::File,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

pub enum Profile {
    Native,
    #[allow(
        dead_code,
        reason = "A64 original source debug-only lifecycle variants"
    )]
    DebugNative,
    Plain,
    Client,
}
struct Running {
    child: Child,
    reaped: bool,
}
impl Drop for Running {
    fn drop(&mut self) {
        if !self.reaped {
            // The native supervisor remains our unreaped child, so this PID
            // cannot be reused. Its TERM handler kills and reaps descendants,
            // including children that created their own process groups.
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
            }
            let _ = self.child.wait();
        }
    }
}
pub fn fixtures() -> PathBuf {
    PathBuf::from(
        std::env::var_os("SEEDBOX_DYNAMORIO_FIXTURES")
            .expect("set SEEDBOX_DYNAMORIO_FIXTURES to prepared matching DR fixtures"),
    )
    .canonicalize()
    .expect("prepared DR fixture directory")
}
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}
/// `isolate` runs outside drrun; its child is the actual application/runtime
/// image. In particular no resident init is interposed before PID1 execution.
pub fn run(binary: &str, args: &[&str], profile: Profile, isolate: Option<&[&str]>) -> Output {
    run_with_options(binary, args, profile, isolate, &[], None)
}
#[allow(
    dead_code,
    reason = "architecture-specific source model variants use explicit public DR options"
)]
pub fn run_with_options(
    binary: &str,
    args: &[&str],
    profile: Profile,
    isolate: Option<&[&str]>,
    options: &[&str],
    client: Option<(&str, &[&str])>,
) -> Output {
    let root = if matches!(profile, Profile::DebugNative) {
        PathBuf::from(
            std::env::var_os("SEEDBOX_DYNAMORIO_DEBUG_FIXTURES")
                .expect("set SEEDBOX_DYNAMORIO_DEBUG_FIXTURES to actual matching Debug fixtures"),
        )
        .canonicalize()
        .expect("prepared matching Debug fixture directory")
    } else {
        fixtures()
    };
    let logs = tempfile::tempdir().unwrap();
    let out = logs.path().join("stdout");
    let err = logs.path().join("stderr");
    let mut command = Command::new(root.join("bin/unix-supervisor"));
    // SysV IPC outlives a crashed process. Keep its native owner outside DR,
    // under the existing subreaper, so failure/containment also removes it.
    if binary == "restart_matrix" && args.first() == Some(&"semop") {
        command.arg("--sysv-sem");
    }
    if let Some(flags) = isolate {
        let launcher = if flags.first() == Some(&"pid1") {
            std::env::var_os("SEEDBOX_DYNAMORIO_PID1_LAUNCHER")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join("bin/seedbox-dr-isolate"))
        } else {
            root.join("bin/seedbox-dr-isolate")
        };
        command.arg(launcher).args(flags).arg("--");
    }
    if !matches!(profile, Profile::Native | Profile::DebugNative) {
        command
            .arg(root.join("runtime/bin64/drrun"))
            .arg("-use_dll")
            .arg(root.join("runtime/lib64/release/libdynamorio.so"));
        command.args(options);
        if matches!(profile, Profile::Client) {
            let (library, arguments) = client.unwrap_or(("libunix-observe.so", &[]));
            command
                .arg("-c")
                .arg(root.join("lib").join(library))
                .args(arguments);
        }
        command.arg("--");
    }
    command
        .arg(root.join(format!("bin/unix-{binary}")))
        .args(args)
        .env_clear()
        .env("DYNAMORIO_CONFIGDIR", logs.path())
        .current_dir(logs.path())
        .stdin(Stdio::null())
        .stdout(File::create(&out).unwrap())
        .stderr(File::create(&err).unwrap());
    if let Some(delegated) = std::env::var_os("SEEDBOX_DYNAMORIO_CGROUP_ROOT") {
        command.env("SEEDBOX_DYNAMORIO_CGROUP_ROOT", delegated);
    }
    let mut running = Running {
        child: command
            .guarded_spawn()
            .expect("start owned Unix fixture supervisor"),
        reaped: false,
    };
    let guard = HangGuard::new(
        "source-derived Unix DR fixture must complete",
        Duration::from_secs(180),
    );
    let status = guard.wait_for(|| running.child.try_wait().transpose());
    if matches!(&status, Ok(Ok(_))) {
        running.reaped = true;
    }
    drop(running);
    let stdout = std::fs::read_to_string(out).unwrap();
    let stderr = std::fs::read_to_string(err).unwrap();
    let status = status
        .unwrap_or_else(|e| panic!("{binary} {args:?}: {e}\n{stdout}\n{stderr}"))
        .expect("wait for Unix fixture");
    Output {
        stdout,
        stderr,
        code: status
            .code()
            .expect("native supervisor reports child signal as 128+signal"),
    }
}
#[allow(
    dead_code,
    reason = "shared runner is also compiled by isolated_cases, which uses run directly"
)]
pub fn checked(binary: &str, args: &[&str], profile: Profile, images: usize) -> Output {
    let client = matches!(profile, Profile::Client);
    let out = run(binary, args, profile, None);
    assert_eq!(
        out.code, 0,
        "{binary} {args:?}\n{}\n{}",
        out.stdout, out.stderr
    );
    assert!(
        !out.stdout.lines().any(|line| line
            .split_whitespace()
            .any(|word| matches!(word, "UNSUPPORTED" | "FAIL" | "BAD"))),
        "fixture must execute its invariant: {}",
        out.stdout
    );
    if binary == "restart_matrix" && args.first() == Some(&"semop") {
        assert!(
            out.stderr
                .lines()
                .any(|s| s == "restart-owner: reaped=1 removed=1"),
            "native SysV owner must reap and remove its resource: {}",
            out.stderr
        );
    }
    if client {
        let executed = out
            .stderr
            .lines()
            .filter(|s| s.starts_with("unix-client: pid=") && s.ends_with(" executed=1"))
            .count();
        assert!(
            executed >= images,
            "{binary} {args:?}: per-image/fork application execution: expected >= {images}, got {executed}\n{}",
            out.stderr
        );
    }
    out
}
#[allow(
    dead_code,
    reason = "shared runner is also compiled by isolated_cases, which uses run directly"
)]
pub fn triplet(binary: &str, args: &[&str], images: usize, required: &[&str]) {
    let native = checked(binary, args, Profile::Native, 0);
    for field in required {
        assert!(
            native.stdout.contains(field),
            "native {binary} {args:?} missing {field:?}: {}",
            native.stdout
        );
    }
    for profile in [Profile::Plain, Profile::Client] {
        let translated = checked(binary, args, profile, images);
        assert_eq!(
            translated.stdout, native.stdout,
            "{binary} {args:?}\n{}",
            translated.stderr
        );
    }
}
