//! Source-derived public translator cases. Inventory gaps are not green tests.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

struct Running {
    child: std::process::Child,
    reaped: bool,
}
impl Drop for Running {
    fn drop(&mut self) {
        if !self.reaped {
            // Keep the native subreaper alive to collect nested/new-group children.
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let _ = self.child.wait();
        }
    }
}

fn output_reader(
    mut pipe: impl Read + Send + 'static,
    live: bool,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0; 8192];
        let mut total = 0;
        loop {
            let n = pipe.read(&mut chunk).expect("read fixture output");
            if n == 0 {
                break;
            }
            // Preserve complete oracle input up to a finite bound; exceeding it
            // fails explicitly, never accepts a truncated result as an oracle.
            let retained = n.min((16 * 1024 * 1024usize).saturating_sub(output.len()));
            output.extend_from_slice(&chunk[..retained]);
            if live && total < 1024 * 1024 {
                let shown = n.min(1024 * 1024 - total);
                std::io::stderr()
                    .write_all(&chunk[..shown])
                    .expect("stream phase diagnostics");
                if shown < n {
                    eprintln!("phase live diagnostics truncated at 1 MiB");
                }
            }
            total += n;
        }
        assert!(total <= 16 * 1024 * 1024, "fixture output exceeded 16 MiB");
        output
    })
}

fn fixtures() -> PathBuf {
    PathBuf::from(std::env::var_os("SEEDBOX_DYNAMORIO_FIXTURES").expect("prepared DR fixtures"))
        .canonicalize()
        .expect("fixture root")
}

fn checked(command: Command, label: &str, expected: &str) -> (String, String) {
    let temporary = tempfile::tempdir().unwrap();
    let python = command.get_program().to_string_lossy().ends_with("python3");
    let mut supervisor = Command::new(fixtures().join("bin/unix-supervisor"));
    supervisor
        .arg(command.get_program())
        .args(command.get_args())
        .env_clear()
        .env("HOME", temporary.path())
        .env("DYNAMORIO_CONFIGDIR", temporary.path())
        .current_dir(temporary.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Running {
        child: supervisor
            .guarded_spawn()
            .expect("start prepared fixture supervisor"),
        reaped: false,
    };
    let stdout = output_reader(child.child.stdout.take().unwrap(), python);
    let stderr = output_reader(child.child.stderr.take().unwrap(), python);
    let seconds = if python { 2400 } else { 300 };
    let guard = HangGuard::new(
        "source-derived fixture completion",
        Duration::from_secs(seconds),
    );
    let result = guard.wait_for(|| child.child.try_wait().transpose());
    child.reaped = matches!(&result, Ok(Ok(_)));
    drop(child);
    let out = String::from_utf8(stdout.join().expect("stdout reader")).unwrap();
    let err = String::from_utf8(stderr.join().expect("stderr reader")).unwrap();
    let status = result
        .unwrap_or_else(|e| panic!("{label}: {e}\n{out}\n{err}"))
        .unwrap();
    assert!(status.success(), "{label}: {status}\n{out}\n{err}");
    assert!(
        out.contains(expected),
        "{label}: missing {expected}\n{out}\n{err}"
    );
    (out, err)
}

fn translated(binary: &str, client: Option<&str>, args: &[&str]) -> Command {
    let root = fixtures();
    let mut cmd = Command::new(root.join("runtime/bin64/drrun"));
    cmd.arg("-use_dll")
        .arg(root.join("runtime/lib64/release/libdynamorio.so"));
    if let Some(client) = client {
        cmd.arg("-c").arg(root.join("lib").join(client)).args(args);
    }
    cmd.arg("--").arg(root.join("bin").join(binary));
    cmd
}

#[test]
fn flush_public_contracts_shared_isa() {
    checked(
        Command::new(fixtures().join("bin/translator_flush")),
        "flush native",
        "256 values checked",
    );
    checked(
        translated("translator_flush", None, &[]),
        "flush plain",
        "256 values checked",
    );
    for mode in [
        "full",
        "full_ex",
        "unlink",
        "delay_cb",
        "delay_nocb",
        "full_unexec",
        "delay_unexec",
    ] {
        let (_, err) = checked(
            translated(
                "translator_flush",
                Some("libtranslator_flush_client.so"),
                &[mode],
            ),
            mode,
            "256 values checked",
        );
        assert!(
            err.contains(&format!("mode={mode} entries=256")),
            "{mode}: {err}"
        );
    }
}

#[test]
fn memcache_failed_mprotect_and_finite_concurrent_unmaps() {
    checked(
        Command::new(fixtures().join("bin/translator_memcache")),
        "memcache native",
        "finite=4096",
    );
    checked(
        translated("translator_memcache", None, &[]),
        "memcache plain",
        "finite=4096",
    );
    let (_, err) = checked(
        translated(
            "translator_memcache",
            Some("libtranslator_memcache_client.so"),
            &[],
        ),
        "memcache client",
        "finite=4096",
    );
    assert!(
        err.contains("memcache-client: unmapped queries=10"),
        "{err}"
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_source_decoder_expectations() {
    let root = fixtures();
    let mut cmd = Command::new(root.join("bin/translator_codec"));
    cmd.arg(root.join("share/translator/codec_vectors.tsv"));
    checked(cmd, "5079 source decoder expectations", ": PASS");
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_native_hardware_ir_words_and_populations() {
    let root = fixtures();
    for population in ["regress", "templates", "mutants", "random"] {
        let mut cmd = Command::new(root.join("bin/translator_isa"));
        cmd.arg(population).args(["-seed", "1", "-n", "80000"]);
        let mut files: Vec<_> = std::fs::read_dir(root.join("share/translator/vectors"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        cmd.args(files);
        checked(cmd, population, "PASS");
    }
}

fn python_case(script: &str, args: &[&str]) {
    let root = fixtures();
    let mut cmd = Command::new(root.join("runtimes/python/bin/python3"));
    cmd.arg(root.join("share/translator").join(script))
        .arg(&root)
        .args(args);
    checked(cmd, &args.join("/"), ": PASS");
}

#[test]
fn node_jit_with_debug_runtime_and_public_observer() {
    let root = PathBuf::from(
        std::env::var_os("SEEDBOX_DYNAMORIO_DEBUG_FIXTURES")
            .expect("prepared matching Debug DR fixtures"),
    )
    .canonicalize()
    .expect("Debug fixture root");
    let mut cmd = Command::new(root.join("runtimes/python/bin/python3"));
    cmd.arg(root.join("share/translator/languages/run.py"))
        .arg(&root)
        .args(["js", "jit", "--debug"]);
    checked(cmd, "Debug js/jit", "SB-runtime:Js.jit: PASS");
}

#[test]
fn jvm_original_model_and_jit_floors() {
    for case in [
        "ic-seedbox-seed1",
        "ic-default-seed1-ops50000",
        "jfr-default-seed1",
        "redefine-default-seed1",
    ] {
        python_case("jvm/run.py", &[case]);
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn jvm_original_arm_progress_profiles() {
    for case in ["deopt-client-seed1-b50000", "ic-client-seed2-b50000"] {
        python_case("jvm/run.py", &[case]);
    }
}

#[test]
fn managed_runtime_source_models() {
    for (language, cases) in [
        ("js", &["wasm", "jit", "workersToken"][..]),
        ("lua", &["trace", "ffi"][..]),
        (
            "go",
            &[
                "preempt",
                "stack",
                "gc",
                "exec",
                "faultRecovery",
                "cgoThreads",
            ][..],
        ),
        ("erl", &["reductions", "heap", "hotCode"][..]),
    ] {
        for case in cases {
            python_case("languages/run.py", &[language, case]);
        }
    }
}

// Separate selectors keep the expensive reverse P/Invoke population independently selectable.
#[test]
fn clr_original_tier() {
    python_case("languages/run.py", &["cs", "tier"]);
}

#[test]
fn clr_original_gc() {
    python_case("languages/run.py", &["cs", "gc"]);
}

#[test]
fn clr_original_reverse_pinvoke() {
    python_case("languages/run.py", &["cs", "reversePInvoke"]);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_exclusive_source_strict_algorithms_and_scheduled_aba() {
    let root = fixtures();
    let mut native = Command::new(root.join("bin/translator_exclusive"));
    native.args(["native", "1", "1200"]);
    checked(native, "exclusive native", "all properties hold");
    for mode in ["cas", "nocas"] {
        for client in [false, true] {
            let mut cmd = Command::new(root.join("runtime/bin64/drrun"));
            cmd.arg("-use_dll")
                .arg(root.join("runtime/lib64/release/libdynamorio.so"))
                .arg(if mode == "cas" {
                    "-ldstex2cas"
                } else {
                    "-no_ldstex2cas"
                });
            if client {
                cmd.arg("-c").arg(root.join("lib/libtranslator_observe.so"));
            }
            cmd.arg("--")
                .arg(root.join("bin/translator_exclusive"))
                .args([mode, "1", "1200"]);
            let (_, err) = checked(
                cmd,
                &format!("exclusive {mode} client={client}"),
                "all properties hold",
            );
            if client {
                assert!(err.contains("path-cost: exit app="), "{err}");
            }
        }
    }
}

#[test]
fn jvm_shared_deopt_model_and_executed_vm_instrumentation() {
    python_case("jvm/run.py", &["deopt-shared-diagnostic"]);
}
