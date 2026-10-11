mod support;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn probe(root: &Path, name: &str, source: &str) -> std::path::PathBuf {
    let input = root.join(format!("{name}.c"));
    let binary = root.join(name);
    fs::write(&input, source).unwrap();
    support::success(
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-std=c11",
                "-D_POSIX_C_SOURCE=200809L",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-I",
            ])
            .arg(support::root().join("guest"))
            .arg(input)
            .arg("-o")
            .arg(&binary),
    );
    binary
}

#[test]
fn wall_clock_retains_subseconds_world_time_and_large_epochs_across_reboots() {
    let directory = tempfile::tempdir().unwrap();
    let binary = probe(
        directory.path(),
        "clock",
        r#"
#include "world-clock.h"
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    if (argc != 3) return 2;
    struct timespec time = seedbox_wall_clock_at(strtoull(argv[1], NULL, 10), strtoull(argv[2], NULL, 10));
    printf("%llu %ld\n", (unsigned long long)time.tv_sec, time.tv_nsec);
}
"#,
    );
    for (epoch, elapsed, expected) in [
        (86400, 999999999, "86400 999999999\n"),
        (86400, 990560000001, "87390 560000001\n"),
        (u32::MAX as u64, 1000000001, "4294967296 1\n"),
        (u32::MAX as u64, u64::MAX, "22741711368 709551615\n"),
    ] {
        let output =
            support::success(Command::new(&binary).args([epoch.to_string(), elapsed.to_string()]));
        assert_eq!(output.stdout, expected.as_bytes());
    }
}

#[test]
fn script_entrypoints_preserve_literal_arguments_and_reject_unexecutable_or_recursive_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let binary = probe(
        root,
        "launch",
        r#"
#include "progress-launch.h"
int main(int argc, char **argv) {
    (void)argc;
    char **resolved = seedbox_script_arguments(argv + 1);
    if (!resolved) { printf("errno=%d\n", errno); return 2; }
    for (; *resolved; ++resolved) puts(*resolved);
}
"#,
    );
    let interpreter = root.join("interpreter");
    fs::write(&interpreter, [b"\x7fELF".as_slice(), &[0; 20]].concat()).unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755)).unwrap();
    let script = root.join("entrypoint");
    fs::write(
        &script,
        format!(
            "#!  {}\targument with spaces  \nignored\n",
            interpreter.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str]| {
        Command::new(&binary)
            .current_dir(root)
            .env("PATH", root)
            .args(args)
            .output()
            .unwrap()
    };
    let output = run(&["entrypoint", "literal $arg", "two words"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}\nargument with spaces\n{}\nliteral $arg\ntwo words\n",
            interpreter.display(),
            script.display()
        )
    );
    assert_eq!(
        run(&[interpreter.to_str().unwrap(), "original"]).stdout,
        format!("{}\noriginal\n", interpreter.display()).as_bytes()
    );
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(run(&[script.to_str().unwrap()]).status.code(), Some(2));
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    for text in [
        format!("#!{}\n", script.display()),
        format!("#!{}", "x".repeat(300)),
    ] {
        fs::write(&script, text).unwrap();
        assert_eq!(run(&[script.to_str().unwrap()]).status.code(), Some(2));
    }
    assert_eq!(run(&["missing"]).status.code(), Some(2));
}

#[test]
fn progress_options_require_exact_order_and_positive_u32_decimals() {
    let directory = tempfile::tempdir().unwrap();
    let binary = probe(
        directory.path(),
        "budget",
        r#"
#include "progress/budget.h"
#include <stdio.h>
int main(int argc, char **argv) {
    uint32_t budget = 0, points = 0;
    if (!seedbox_progress_arguments(argc, (const char **)argv, &budget, &points)) return 2;
    printf("%u %u\n", budget, points);
}
"#,
    );
    let budget = "--instructions-per-progress-call";
    let points = "--progress-points-per-block";
    for value in ["1", "17", "50000", "4294967295"] {
        let output = support::success(Command::new(&binary).args([budget, value, points, value]));
        assert_eq!(output.stdout, format!("{value} {value}\n").as_bytes());
    }
    let rejects = |args: &[&str]| {
        assert_eq!(
            Command::new(&binary)
                .args(args)
                .output()
                .unwrap()
                .status
                .code(),
            Some(2),
            "{args:?}"
        );
    };
    for value in [
        "",
        "0",
        "-1",
        "+1",
        " 1",
        "1 ",
        "1x",
        "4294967296",
        "999999999999999999999999999999",
    ] {
        rejects(&[budget, value, points, "1"]);
        rejects(&[budget, "1", points, value]);
    }
    for args in [
        vec![budget, "1"],
        vec![points, "1", budget, "1"],
        vec![budget, "1", points, "1", "--other", "1"],
        vec!["--blocks-per-progress-call", "1", points, "1"],
        vec!["--instructions-per-progress-call-extra", "1", points, "1"],
        vec!["--instructions-per-progress-cal", "1", points, "1"],
    ] {
        rejects(&args);
    }
}
