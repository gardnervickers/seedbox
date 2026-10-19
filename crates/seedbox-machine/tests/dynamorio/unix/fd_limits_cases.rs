//! Cooperative resource contract: real kernel limits, ordinary fork/exec and
//! explicit mandatory-resource errors. Strict low-FD transparency is superseded.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{
    fs::File,
    io::Write,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
enum Profile {
    Native,
    Release,
    Debug,
}
struct Running {
    child: Child,
    reaped: bool,
}
impl Drop for Running {
    fn drop(&mut self) {
        if !self.reaped {
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
            }
            let _ = self.child.wait();
        }
    }
}
struct Output {
    code: i32,
    out: String,
    err: String,
    retained: Option<Vec<u8>>,
}
fn run(profile: Profile, binary: &str, args: &[&str], soft: &str, boundary: bool) -> Output {
    let debug = matches!(profile, Profile::Debug);
    let key = if debug {
        "SEEDBOX_DYNAMORIO_DEBUG_FIXTURES"
    } else {
        "SEEDBOX_DYNAMORIO_FIXTURES"
    };
    let root = PathBuf::from(
        std::env::var_os(key).unwrap_or_else(|| panic!("set {key} to matching prepared fixtures")),
    );
    let axis = format!("{profile:?} {binary} {args:?} host-soft={soft}");
    let logs = tempfile::tempdir().unwrap();
    let stdout = logs.path().join("stdout");
    let stderr = logs.path().join("stderr");
    let mut command = Command::new(root.join("bin/unix-supervisor"));
    command
        .arg(root.join("bin/unix-fd-limits-launch"))
        .arg(soft);
    if !matches!(profile, Profile::Native) {
        let build = if debug { "debug" } else { "release" };
        command.arg(root.join("runtime/bin64/drrun"));
        if debug {
            command.arg("-debug");
        }
        command
            .arg("-use_dll")
            .arg(root.join(format!("runtime/lib64/{build}/libdynamorio.so")));
        if boundary {
            command.arg("-c").arg(root.join("lib/libunix-fd-limits.so"));
        }
        command.arg("--");
    }
    let application = if boundary {
        "unix-rlimit_nofile".to_owned()
    } else {
        format!("unix-fd-limits-{binary}")
    };
    command
        .arg(root.join("bin").join(application))
        .args(args)
        .env_clear()
        .env("DYNAMORIO_CONFIGDIR", logs.path())
        .current_dir(logs.path())
        .stdin(if boundary {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut running = Running {
        child: command.guarded_spawn().expect("start owned FD supervisor"),
        reaped: false,
    };
    let guard = HangGuard::new("FD fixture positive completion", Duration::from_secs(30));
    if boundary {
        // The app announces its PID only once its own public kernel checks pass.
        // A regular log file cannot fill a pipe; readiness is a positive record.
        let pid = guard
            .wait_for(|| {
                let out = std::fs::read_to_string(&stdout).unwrap();
                out.lines().find_map(|line| {
                    line.strip_prefix("READY ")
                        .map(|pid| pid.parse::<libc::pid_t>().unwrap())
                })
            })
            .unwrap_or_else(|error| {
                panic!(
                    "{axis}: {error}\n{}",
                    std::fs::read_to_string(&stderr).unwrap()
                )
            });
        let mut parent: libc::rlimit = unsafe { std::mem::zeroed() };
        let mut observed: libc::rlimit = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut parent) },
            0
        );
        assert_eq!(
            unsafe { libc::prlimit(pid, libc::RLIMIT_NOFILE, std::ptr::null(), &mut observed) },
            0
        );
        assert_eq!(observed.rlim_cur, 8192, "{axis}: no automatic raise");
        assert_eq!(
            observed.rlim_max, parent.rlim_max,
            "{axis}: hard limit unchanged"
        );
        let next = libc::rlimit {
            rlim_cur: 6144,
            rlim_max: observed.rlim_max,
        };
        assert_eq!(
            unsafe { libc::prlimit(pid, libc::RLIMIT_NOFILE, &next, std::ptr::null_mut()) },
            0
        );
        running.child.stdin.take().unwrap().write_all(b"x").unwrap();
    }
    let status = guard.wait_for(|| running.child.try_wait().transpose());
    if matches!(&status, Ok(Ok(_))) {
        running.reaped = true;
    }
    drop(running);
    let out = std::fs::read_to_string(&stdout).unwrap();
    let err = std::fs::read_to_string(&stderr).unwrap();
    let status = status
        .unwrap_or_else(|error| panic!("{axis}: {error}\n{out}\n{err}"))
        .expect("wait for fixture");
    let code = status
        .code()
        .expect("supervisor converts signals to exit codes");
    eprintln!("{axis}: exit={code}\n{out}\n{err}");
    Output {
        code,
        out,
        err,
        retained: std::fs::read(logs.path().join("retained")).ok(),
    }
}
fn maps_shortage(output: &Output) -> bool {
    output.code == 40
        && output
            .err
            .contains("Unable to open required memory map /proc/")
        && output.err.contains("/maps: error 24")
}
#[test]
fn fd_limits_kernel_visibility_and_instrumented_fork_exec() {
    for profile in [Profile::Native, Profile::Release, Profile::Debug] {
        let output = run(profile, "normal", &["boundary"], "8192", true);
        assert_eq!(output.code, 0, "{}", output.err);
        assert!(output.out.contains(
            "exec'd limit 2048\nlimits kernel-visible set/get/prlimit fork/exec preserved\nOK\n"
        ));
        if !matches!(profile, Profile::Native) {
            let initialized = output
                .err
                .lines()
                .filter(|line| line.starts_with("fd-client: init="))
                .count();
            assert_eq!(
                initialized, 2,
                "parent and exec child must initialize instrumentation"
            );
            let receipts: Vec<_> = output
                .err
                .lines()
                .filter_map(|line| line.strip_prefix("fd-client: pid="))
                .collect();
            assert_eq!(
                receipts.len(),
                2,
                "parent and exec child must complete instrumentation"
            );
            assert!(receipts.iter().all(|line| {
                line.split_once(" blocks=")
                    .unwrap()
                    .1
                    .parse::<u64>()
                    .unwrap()
                    > 0
            }));
        }
    }
}
#[test]
fn fd_limits_kernel_failed_truncate_preserves_data_or_reports_resource_failure() {
    for profile in [Profile::Native, Profile::Release, Profile::Debug] {
        let output = run(profile, "truncate", &[], "8192", false);
        assert_eq!(
            output.retained.as_deref(),
            Some(b"keep-data".as_slice()),
            "retained bytes after application or runtime exit"
        );
        if maps_shortage(&output) && !matches!(profile, Profile::Native) {
            eprintln!(
                "{profile:?}: explicit mandatory-resource failure; O_TRUNC workload completion unqualified"
            );
        } else {
            assert_eq!(output.code, 0, "{}", output.err);
            assert_eq!(
                output.out,
                format!(
                    "fd-limits truncate: open=-1 errno={} retained_bytes=9 contents-match=1\n",
                    libc::EMFILE
                )
            );
        }
    }
}
#[test]
fn fd_limits_usable_signalfd_survives_limit_change() {
    #[cfg(target_arch = "x86_64")]
    let forms = ["old", "new"];
    #[cfg(target_arch = "aarch64")]
    let forms = ["new"];
    for profile in [Profile::Native, Profile::Release, Profile::Debug] {
        for form in forms {
            let output = run(profile, "signalfd", &[form, "update"], "8192", false);
            assert_eq!(output.code, 0, "{}", output.err);
            assert_eq!(
                output.out,
                format!(
                    "fd-limits signalfd: {form} update existing-fd flags and signal delivery preserved\n"
                )
            );
        }
    }
}
#[test]
fn fd_limits_low_host_setup_reports_required_resource_error() {
    for profile in [Profile::Release, Profile::Debug] {
        let output = run(profile, "truncate", &[], "4", false);
        assert!(
            maps_shortage(&output),
            "explicit required-maps error, not app success or a signal: {}\n{}",
            output.out,
            output.err
        );
        assert!(!output.out.contains("contents-match=1"));
    }
}
