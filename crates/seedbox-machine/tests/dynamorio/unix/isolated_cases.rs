//! Genuine namespace-init and owned pids-cgroup regressions. The named
//! dynamorio-isolated suite must preflight capabilities; none is a skip.
#[path = "runner.rs"]
mod runner;
use runner::{Profile, run};
fn image_executed(stderr: &str) {
    assert!(
        stderr
            .lines()
            .any(|s| s.starts_with("unix-client: pid=") && s.ends_with(" executed=1")),
        "client must execute this application image: {stderr}"
    );
}
#[test]
fn namespace_init_default_signal_and_real_fault_lifecycle() {
    for profile in [Profile::Native, Profile::Plain, Profile::Client] {
        // Fresh namespace each time. A caught signal and each fatal instruction
        // have separate images and exit status, not one batched process.
        let client = matches!(profile, Profile::Client);
        let out = run("pid1", &["self-default"], profile, Some(&["pid1"]));
        assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
        assert_eq!(out.stdout, "OK pid1: still alive after raise(SIGSEGV)\n");
        if client {
            image_executed(&out.stderr);
        }
    }
    for profile in [Profile::Native, Profile::Plain, Profile::Client] {
        let client = matches!(profile, Profile::Client);
        let out = run("pid1", &["caught"], profile, Some(&["pid1"]));
        assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
        assert_eq!(out.stdout, "OK pid1: handler received SIGUSR1\n");
        if client {
            image_executed(&out.stderr);
        }
    }
    for (mode, sig) in [
        ("segv", libc::SIGSEGV),
        ("ill", libc::SIGILL),
        ("trap", libc::SIGTRAP),
    ] {
        for profile in [Profile::Native, Profile::Plain, Profile::Client] {
            let client = matches!(profile, Profile::Client);
            let out = run("pid1", &[mode], profile, Some(&["pid1"]));
            assert_eq!(
                out.code,
                128 + sig,
                "{mode}: {}\n{}",
                out.stdout,
                out.stderr
            );
            if client {
                image_executed(&out.stderr);
            }
        }
    }
    for profile in [Profile::Native, Profile::Plain, Profile::Client] {
        let out = run("pid1", &["non-init-default"], profile, None);
        assert_eq!(
            out.code,
            128 + libc::SIGSEGV,
            "{}\n{}",
            out.stdout,
            out.stderr
        );
    }
}
#[test]
fn real_pids_eagain_rolls_back_creation_and_recovers_capacity() {
    for profile in [Profile::Native, Profile::Plain, Profile::Client] {
        let client = matches!(profile, Profile::Client);
        let out = run("eagain", &[], profile, Some(&["pids", "--limit", "32"]));
        assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
        assert_eq!(
            out.stdout,
            "OK eagain: fillers acknowledged, EAGAIN, flush 100/100, joined, thread recovered, clone3 exit 7\n"
        );
        let hits = out.stderr.lines().find_map(|line| {
            line.strip_prefix("isolate:pids-limit-hits=")
                .and_then(|n| n.parse::<u64>().ok())
        });
        assert!(
            hits.is_some_and(|n| n > 0),
            "must attribute EAGAIN to owned pids.max: {}",
            out.stderr
        );
        if client {
            image_executed(&out.stderr);
            assert!(
                out.stderr.contains(" resumed=1 flushed=1"),
                "pre-existing helper must be suspended and resumed: {}",
                out.stderr
            );
        }
    }
}

#[test]
fn original_nested_pid_namespace_reaps_orphans_and_rejects_fork_after_init_exit() {
    for profile in [Profile::Native, Profile::Plain, Profile::Client] {
        let out = run("properties", &["pidns"], profile, Some(&["pid1"]));
        assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
        for field in [
            "init 1 0 1",
            "orphanParent 1",
            "reaped 2 14",
            "initExit 3",
            "rounds 3/3",
        ] {
            assert!(
                out.stdout.contains(field),
                "missing {field}: {}",
                out.stdout
            );
        }
    }
}
