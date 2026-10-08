//! Discover Compose workloads; the public CLI and guest checks own each verdict.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use std::{fs, path::Path, process::Command};

fn invoke(cli: &Path, directory: &Path, name: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(cli);
    command.args(args).env_remove("SEEDBOX_VMM");
    if args.first() != Some(&"diff") {
        command.arg("--state").arg(directory.join("state"));
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if let Some(cpu) = std::env::var_os("SEEDBOX_TEST_CPU") {
            command.arg("--cpu").arg(cpu);
        }
        // Counter contracts also run on Intel's experimental counter profile.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if std::env::var("SEEDBOX_TEST_COUNTER").as_deref() == Ok("1") {
            command.arg("--allow-unqualified-cpu");
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("{}: {error}", cli.display()))?;
    let mut log = output.stdout.clone();
    log.extend_from_slice(&output.stderr);
    let logfile = directory.join(format!("{name}.log"));
    fs::write(&logfile, log).map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "{name}: {}; see {}",
            output.status,
            logfile.display()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_directory(output: &str) -> Result<&str, String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("PASS: "))
        .ok_or_else(|| format!("missing PASS run directory: {output}"))
}

#[test]
fn every_application_runs_twice_and_replays() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let workloads =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/workloads");
    let mut workloads: Vec<_> = fs::read_dir(workloads)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    workloads.sort();
    assert!(!workloads.is_empty());
    let timebases = if std::env::var("SEEDBOX_TEST_COUNTER").as_deref() == Ok("1") {
        vec!["pv", "counter"]
    } else {
        vec!["pv"]
    };
    let mut failures = Vec::new();
    for workload in workloads {
        for timebase in &timebases {
            let directory = Path::new(&reports)
                .join("applications")
                .join(workload.file_stem().unwrap())
                .join(timebase);
            fs::create_dir_all(&directory).unwrap();
            let check = || -> Result<(), String> {
                let run = [
                    "run",
                    workload.to_str().unwrap(),
                    "--timebase",
                    timebase,
                    "--full-state",
                ];
                let first = invoke(Path::new(&cli), &directory, "first", &run)?;
                let second = invoke(Path::new(&cli), &directory, "second", &run)?;
                let first = run_directory(&first)?;
                let second = run_directory(&second)?;
                invoke(
                    Path::new(&cli),
                    &directory,
                    "diff",
                    &["diff", first, second],
                )?;
                invoke(Path::new(&cli), &directory, "replay", &["replay", first])?;
                Ok(())
            };
            if let Err(error) = check() {
                failures.push(format!("{} ({timebase}): {error}", workload.display()));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
