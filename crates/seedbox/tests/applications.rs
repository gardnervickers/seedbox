//! Discover Compose workloads; the public CLI and guest checks own each verdict.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use std::{fs, path::Path, process::Command};

fn invoke(cli: &Path, directory: &Path, name: &str, args: &[&str]) -> Result<String, String> {
    let logfile = directory.join(format!("{name}.log"));
    let log = fs::File::create(&logfile).map_err(|error| error.to_string())?;
    eprintln!("application: {} / {name}", directory.display());
    let mut command = Command::new(cli);
    command
        .args(args)
        .env_remove("SEEDBOX_VMM")
        .stdout(log.try_clone().map_err(|error| error.to_string())?)
        .stderr(log);
    if args.first() != Some(&"diff") {
        command.arg("--state").arg(directory.join("state"));
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if let Some(cpu) = std::env::var_os("SEEDBOX_TEST_CPU") {
            command.arg("--cpu").arg(cpu);
        }
    }
    let status = command
        .status()
        .map_err(|error| format!("{}: {error}", cli.display()))?;
    if !status.success() {
        return Err(format!("{name}: {status}; see {}", logfile.display()));
    }
    Ok(
        String::from_utf8_lossy(&fs::read(&logfile).map_err(|error| error.to_string())?)
            .into_owned(),
    )
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
            // Each preparation records its own guest wall-clock epoch unless
            // --build-epoch pins it, which only native Linux preparation accepts.
            // runtime-tls checks a certificate minted during preparation against
            // that recorded epoch, so a pinned past epoch would predate it.
            let pinned = cfg!(all(target_os = "linux", target_arch = "x86_64"))
                && workload.file_stem().unwrap() != "runtime-tls";
            let check = || -> Result<(), String> {
                let mut run = vec![
                    "run",
                    workload.to_str().unwrap(),
                    "--timebase",
                    timebase,
                    "--memory-mb",
                    "2048",
                    "--full-state",
                ];
                if pinned {
                    run.extend(["--build-epoch", "0"]);
                }
                let first = invoke(Path::new(&cli), &directory, "first", &run)?;
                let second = invoke(Path::new(&cli), &directory, "second", &run)?;
                let first = run_directory(&first)?;
                let second = run_directory(&second)?;
                // Runs with different epochs differ, so each must replay instead.
                if pinned {
                    invoke(
                        Path::new(&cli),
                        &directory,
                        "diff",
                        &["diff", first, second],
                    )?;
                }
                invoke(Path::new(&cli), &directory, "replay", &["replay", first])?;
                if !pinned {
                    invoke(
                        Path::new(&cli),
                        &directory,
                        "replay-second",
                        &["replay", second],
                    )?;
                }
                Ok(())
            };
            if let Err(error) = check() {
                failures.push(format!("{} ({timebase}): {error}", workload.display()));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
