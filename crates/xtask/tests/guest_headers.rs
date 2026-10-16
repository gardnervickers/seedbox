//! Host probes of the actual guest headers; no guest or VM is started.
#![cfg(unix)]
mod support;

use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::{ffi::OsStrExt, ffi::OsStringExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};
use support::{diagnostic, root, run, success};

#[path = "support/cc_words.rs"]
mod cc_words;
use cc_words::{compiler_words, split_compiler as progress_compiler};

fn compile(directory: &Path, source: &str, mut command: Command) -> PathBuf {
    let source_path = directory.join("probe.c");
    let executable = directory.join("probe");
    fs::write(&source_path, source).unwrap();
    success(
        command
            .args(["-Wall", "-Wextra", "-Werror", "-I"])
            .arg(root().join("guest"))
            .arg(&source_path)
            .arg("-o")
            .arg(&executable),
        120,
    );
    executable
}

fn code(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{}",
        diagnostic(output)
    );
}

#[test]
fn epoch_offset_preserves_subseconds_and_late_reboots() {
    let directory = tempfile::tempdir().unwrap();
    // This original test treats CC as one executable, unlike the two progress
    // tests. Preserve that distinction, including executable paths with spaces.
    let mut compiler = Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()));
    compiler.arg("-std=c11");
    let probe = compile(
        directory.path(),
        include_str!("fixtures/guest-headers/world-clock.c"),
        compiler,
    );
    success(&mut Command::new(&probe), 10);
    drop(directory);
    assert!(!probe.exists(), "host compiler output must be removed");
}

#[test]
fn interpreter_arguments_path_permissions_and_recursion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let probe = compile(
        path,
        include_str!("fixtures/guest-headers/progress-launch.c"),
        progress_compiler(),
    );
    let interpreter = path.join("interpreter");
    let mut elf = b"\x7fELF".to_vec();
    elf.extend([0; 20]);
    seedbox_test::process::install_executable(&interpreter, &elf, 0o755).unwrap();
    let script = path.join("entrypoint");
    seedbox_test::process::install_executable(
        &script,
        format!(
            "#!  {}\targument with spaces  \nignored\n",
            interpreter.display()
        )
        .as_bytes(),
        0o755,
    )
    .unwrap();
    let invoke = |arguments: &[&OsStr]| {
        run(
            Command::new(&probe)
                .args(arguments)
                .current_dir(path)
                .env("PATH", path),
            10,
        )
    };
    let output = invoke(&[
        "entrypoint".as_ref(),
        "literal $arg".as_ref(),
        "two words".as_ref(),
    ]);
    code(&output, 0);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            interpreter.to_str().unwrap(),
            "argument with spaces",
            script.to_str().unwrap(),
            "literal $arg",
            "two words",
        ]
    );
    let output = invoke(&[interpreter.as_os_str(), "original".as_ref()]);
    code(&output, 0);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [interpreter.to_str().unwrap(), "original"]
    );
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    code(&invoke(&[script.as_os_str()]), 2);
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    rewrite(&script, format!("#!{}\n", script.display()));
    code(&invoke(&[script.as_os_str()]), 2);
    rewrite(&script, format!("#!{}", "x".repeat(300)));
    code(&invoke(&[script.as_os_str()]), 2);
    code(&invoke(&["missing".as_ref()]), 2);
    drop(directory);
    assert!(!probe.exists(), "host compiler output must be removed");
}

#[test]
fn strict_budget_parser_accepts_u32_range_and_rejects_bad_options() {
    let directory = tempfile::tempdir().unwrap();
    let mut compiler = progress_compiler();
    compiler.arg("-std=c11");
    let probe = compile(
        directory.path(),
        include_str!("fixtures/guest-headers/progress-budget.c"),
        compiler,
    );
    for budget in ["1", "17", "10000", "4294967295"] {
        let output = run(
            Command::new(&probe).args(["--blocks-per-progress-call", budget]),
            10,
        );
        code(&output, 0);
        assert_eq!(output.stdout, format!("{budget}\n").as_bytes());
    }
    for budget in [
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
        let output = run(
            Command::new(&probe).args(["--blocks-per-progress-call", budget]),
            10,
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{budget:?}: {}",
            diagnostic(&output)
        );
    }
    for option in [
        "--other",
        "--blocks-per-progress-call-extra",
        "--blocks-per-progress-cal",
    ] {
        code(&run(Command::new(&probe).args([option, "1"]), 10), 2);
    }
    drop(directory);
    assert!(!probe.exists(), "host compiler output must be removed");
}

#[test]
fn compiler_words_preserve_quoting_without_expansion_or_comments() {
    for (input, expected) in [
        ("cc", vec!["cc"]),
        (
            "'compiler path' -DNAME=\"two words\"",
            vec!["compiler path", "-DNAME=two words"],
        ),
        ("cc '' \"\"", vec!["cc", "", ""]),
        (
            "cc #literal $HOME $(command)",
            vec!["cc", "#literal", "$HOME", "$(command)"],
        ),
        ("cc \"\\$value\\`\\q\"", vec!["cc", "\\$value\\`\\q"]),
        ("cc \"a\\\"b\\\\c\"", vec!["cc", "a\"b\\c"]),
        (
            "cc a\\ b 'literal\\word'",
            vec!["cc", "a b", "literal\\word"],
        ),
        ("cc\r\n\targument", vec!["cc", "argument"]),
        ("cc a\\\nb", vec!["cc", "a\nb"]),
        ("  \t", vec![]),
    ] {
        assert_eq!(
            compiler_words(OsStr::new(input)).unwrap(),
            expected.into_iter().map(OsString::from).collect::<Vec<_>>(),
            "{input:?}",
        );
    }
    for input in ["cc \\", "cc 'missing", "cc \"missing"] {
        assert!(compiler_words(OsStr::new(input)).is_err(), "{input:?}");
    }
    assert_eq!(
        compiler_words(OsStr::from_bytes(b"cc '\xff path'")).unwrap(),
        [
            OsString::from("cc"),
            OsString::from_vec(b"\xff path".to_vec())
        ],
    );
}

/// Replace an executable this test runs, with forks excluded until the
/// rewritten file is closed.
fn rewrite(path: &Path, contents: String) {
    let _writing = seedbox_exec::writing();
    fs::write(path, contents).unwrap();
}
