use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{
    fs::File,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(Clone, Copy)]
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
impl Profile {
    fn label(self) -> &'static str {
        match self {
            // Direct launch may enter the statically linked DR itself.
            Self::Native => "native/direct-release",
            Self::DebugNative => "native/static-debug",
            Self::Plain => "plain-release",
            Self::Client => "client-release",
        }
    }
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
    pub profile: &'static str,
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
    let context = format!("{binary} {args:?} profile={}", profile.label());
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
            .unwrap_or_else(|e| panic!("{context}: start owned Unix fixture supervisor: {e}")),
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
        .unwrap_or_else(|e| panic!("{context}: {e}\n{stdout}\n{stderr}"))
        .unwrap_or_else(|e| panic!("{context}: wait for Unix fixture: {e}"));
    Output {
        profile: profile.label(),
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
        "{binary} {args:?} profile={}\n{}\n{}",
        out.profile, out.stdout, out.stderr
    );
    assert!(
        !out.stdout.lines().any(|line| line
            .split_whitespace()
            .any(|word| matches!(word, "UNSUPPORTED" | "FAIL" | "BAD"))),
        "{binary} {args:?} profile={}: fixture must execute its invariant: {}",
        out.profile,
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
            "{binary} {args:?} profile={}: per-image/fork application execution: expected >= {images}, got {executed}\n{}",
            out.profile,
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
            "{binary} {args:?} profile={} compared with profile={}\n{}",
            translated.profile, native.profile, translated.stderr
        );
    }
}

#[test]
fn failure_identifies_selected_profile() {
    const CHILD: &str = "SEEDBOX_UNIX_PROFILE_TEST_CHILD";
    let profiles = [
        Profile::Native,
        Profile::DebugNative,
        Profile::Plain,
        Profile::Client,
    ];
    if let Ok(index) = std::env::var(CHILD) {
        checked(
            "synthetic",
            &["selector"],
            profiles[index.parse::<usize>().unwrap()],
            0,
        );
        panic!("synthetic failure must retain the status assertion");
    }
    let test = concat!(module_path!(), "::failure_identifies_selected_profile")
        .split_once("::")
        .unwrap()
        .1;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("bin")).unwrap();
    seedbox_test::process::install_executable(
        &root.path().join("bin/unix-supervisor"),
        b"#!/bin/sh\nprintf synthetic-output\nprintf synthetic-error >&2\nexit 7\n",
        0o700,
    )
    .unwrap();
    for (index, profile) in profiles.iter().enumerate() {
        let stdout = root.path().join("stdout");
        let stderr = root.path().join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, index.to_string())
            .env("SEEDBOX_DYNAMORIO_FIXTURES", root.path())
            .env("SEEDBOX_DYNAMORIO_DEBUG_FIXTURES", root.path())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        let mut child = Running {
            child: command.guarded_spawn().unwrap(),
            reaped: false,
        };
        let status = HangGuard::new("synthetic profile failure", Duration::from_secs(30))
            .wait_for(|| child.child.try_wait().transpose())
            .unwrap()
            .unwrap();
        child.reaped = true;
        let output = format!(
            "{}{}",
            std::fs::read_to_string(stdout).unwrap(),
            std::fs::read_to_string(stderr).unwrap()
        );
        assert!(!status.success(), "{output}");
        assert!(
            output.contains(&format!(
                "synthetic [\"selector\"] profile={}",
                profile.label()
            )),
            "{output}"
        );
        assert!(
            output.contains("synthetic-output") && output.contains("synthetic-error"),
            "{output}"
        );
        assert!(output.contains("left: 7"), "{output}");
    }
}
