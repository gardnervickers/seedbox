use std::{
    env, fs, io,
    os::unix::{ffi::OsStrExt, fs::symlink, io::AsRawFd},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
mod assets;
mod runtime;

// Spawns and executable writes go through the process-wide seedbox_exec scope
// (runtime.rs writes the QEMU launcher and copies its closure). Production
// build-assets is sequential; the unit tests share one descriptor table.
fn guarded_status(command: &mut Command) -> io::Result<std::process::ExitStatus> {
    seedbox_exec::spawn(command)?.wait()
}
/// Like `Command::output` with stdin null and stdout captured; stderr is
/// whatever the caller set (std's default would capture it).
fn guarded_output(command: &mut Command) -> io::Result<std::process::Output> {
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    seedbox_exec::spawn(command)?.wait_with_output()
}

fn run(command: &mut Command) -> Result<()> {
    let status = guarded_status(
        command
            .stdout(Stdio::from(io::stderr()))
            .stderr(Stdio::inherit()),
    )?;
    if !status.success() {
        return Err(format!("{command:?}: {status}").into());
    }
    Ok(())
}
fn capture(command: &mut Command) -> Result<String> {
    let output = guarded_output(command.stderr(Stdio::inherit()))?;
    if !output.status.success() {
        return Err(format!("{command:?}: {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn command(program: &str, root: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(root);
    command
}
fn linux(root: &Path, output: &Path) -> Result<()> {
    let kernel = capture(command("nix", root).args([
        "build",
        "--no-link",
        "--print-out-paths",
        ".#guestKernel",
    ]))?;
    let runtime = capture(command("nix", root).args([
        "build",
        "--no-link",
        "--print-out-paths",
        ".#runtimeArchive",
    ]))?;
    let image = match env::consts::ARCH {
        "x86_64" => "bzImage",
        "aarch64" => "Image",
        arch => return Err(format!("unsupported architecture {arch}").into()),
    };
    fs::copy(Path::new(&kernel).join(image), output.join("kernel"))?;
    fs::copy(runtime, output.join("runtime.tar.gz"))?;
    runtime::guest(root, &output.join("driver"), env::consts::ARCH)?;
    Ok(())
}
fn mac(root: &Path, output: &Path) -> Result<()> {
    let candidates = env::var("SEEDBOX_ASSET_BUILDER")
        .map(|s| vec![s])
        .unwrap_or_else(|_| vec!["podman".into(), "docker".into()]);
    let client = candidates
        .into_iter()
        .find(|p| {
            guarded_status(
                Command::new(p)
                    .arg("info")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
            )
            .is_ok_and(|s| s.success())
        })
        .ok_or("start Podman or Docker Desktop to build Mac guest inputs")?;
    let image = "docker.io/nixos/nix:latest";
    let mut inspected =
        capture(command(&client, root).args(["image", "inspect", image, "--format", "{{.Id}}"]));
    if inspected.is_err() {
        run(command(&client, root).args(["pull", "--platform", "linux/arm64", image]))?;
        inspected = capture(
            command(&client, root).args(["image", "inspect", image, "--format", "{{.Id}}"]),
        );
    }
    let id = inspected?;
    let id = id.strip_prefix("sha256:").unwrap_or(&id);
    if id.len() < 24 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid builder image ID".into());
    }
    let source = tempfile::tempdir_in(output)?;
    let listed = guarded_output(
        command("git", root)
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .stderr(Stdio::piped()),
    )?;
    if !listed.status.success() {
        return Err("cannot list checkout files".into());
    }
    let mut files = Vec::new();
    for name in listed.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        if fs::symlink_metadata(root.join(std::ffi::OsStr::from_bytes(name))).is_ok() {
            files.extend_from_slice(name);
            files.push(0);
        }
    }
    fs::write(source.path().join("files"), files)?;
    run(command("tar", root)
        .args(["--null", "-T"])
        .arg(source.path().join("files"))
        .arg("-cf")
        .arg(source.path().join("source.tar"))
        .env("COPYFILE_DISABLE", "1"))?;
    let script = r#"mkdir /source /output
 tar -xf - -C /source
 cd /source
 export NIX_CONFIG="experimental-features = nix-command flakes
sandbox = false"
 kernel=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#guestKernel)
 runtime=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#runtimeArchive)
 cp "$kernel/Image" /output/kernel
 cp "$runtime" /output/runtime.tar.gz
 pv=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#machinePvAssets)
 tcg=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#machineTcgAssets)
 formatter=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#machineGuestFormatter)
 formatter_x86=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#machineGuestFormatterX86)
 mkdir -p /output/driver/pv/aarch64 /output/driver/tcg/x86_64
 cp "$pv/kernel" "$pv/init" "$pv/command-line" "$pv/progress.tar" /output/driver/pv/aarch64/
 cp "$tcg/kernel" "$tcg/init" "$tcg/command-line" "$tcg/firmware" /output/driver/tcg/x86_64/
 cp "$formatter/bin/mke2fs" /output/driver/pv/aarch64/mke2fs
 cp "$formatter_x86/bin/mke2fs" /output/driver/tcg/x86_64/mke2fs
"#;
    let container = capture(command(&client, root).args([
        "create",
        "-i",
        "--platform",
        "linux/arm64",
        "--mount",
        &format!(
            "type=volume,source=seedbox-nix-{},destination=/nix",
            &id[..24]
        ),
        image,
        "sh",
        "-eu",
        "-c",
        script,
    ]))?;
    let result = (|| {
        run(command(&client, root)
            .args(["start", "--attach", "--interactive", &container])
            .stdin(fs::File::open(source.path().join("source.tar"))?))?;
        run(command(&client, root)
            .args(["cp", &format!("{container}:/output/.")])
            .arg(output))
    })();
    let cleanup = run(command(&client, root).args(["rm", "--force", &container]));
    result.and(cleanup)
}
fn build(root: &Path) -> Result<PathBuf> {
    publish(root, |output| {
        match (env::consts::OS, env::consts::ARCH) {
            ("linux", "x86_64" | "aarch64") => linux(root, output)?,
            ("macos", "aarch64") => mac(root, output)?,
            _ => return Err("requires Linux x86-64/AArch64 or Apple Silicon macOS".into()),
        }
        let driver = output.join("driver");
        runtime::host(root, &driver)?;
        fs::write(
            driver.join("host-system"),
            format!(
                "{}-{}",
                env::consts::ARCH,
                if cfg!(target_os = "macos") {
                    "darwin"
                } else {
                    "linux"
                }
            ),
        )?;
        runtime::archive(root, &driver, &output.join("driver-runtime.tar.gz"))?;
        fs::remove_dir_all(driver)?;
        Ok(())
    })
}

fn asset_lock(root: &Path) -> Result<fs::File> {
    let directory = root.join("build");
    fs::create_dir_all(&directory)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".lock"))?;
    // SAFETY: the owned lock file stays open through publication.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(lock)
}

fn publish(root: &Path, build_assets: impl FnOnce(&Path) -> Result<()>) -> Result<PathBuf> {
    let _lock = asset_lock(root)?;
    let directory = root.join("build");
    let pending = tempfile::Builder::new()
        .prefix(".assets-")
        .tempdir_in(&directory)?;
    build_assets(pending.path())?;
    fs::write(pending.path().join("architecture"), env::consts::ARCH)?;
    let current = directory.join("assets");
    // A missing or damaged previous bundle must not prevent its replacement.
    if assets::equal(pending.path(), &current).unwrap_or(false) {
        return Ok(current.canonicalize()?);
    }
    let retained = pending.keep();
    let pointer = directory.join(".assets-next");
    if pointer.symlink_metadata().is_ok() {
        fs::remove_file(&pointer)?;
    }
    symlink(
        retained.file_name().ok_or("missing asset directory name")?,
        &pointer,
    )?;
    fs::rename(pointer, directory.join("assets"))?;
    Ok(retained)
}

fn clean_assets(root: &Path) -> Result<()> {
    let _lock = asset_lock(root)?;
    let directory = root.join("build");
    // Fail closed if the current publication cannot be resolved.
    let current = directory.join("assets").canonicalize()?;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        if entry.file_name().as_bytes().starts_with(b".assets-")
            && entry.file_type()?.is_dir()
            && entry.path().canonicalize()? != current
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn build_cli(
    root: &Path,
    args: &[std::ffi::OsString],
    prepare: impl FnOnce() -> Result<PathBuf>,
) -> Result<()> {
    prepare()?;
    run(command("cargo", root).arg("build").args(args))
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let usage = "usage: cargo xtask build [CARGO BUILD OPTIONS] | build-assets | clean-assets\nclean-assets removes old generations; stop all Cargo builds before running it";
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    match args.split_first() {
        Some((action, rest)) if action == "build-assets" && rest.is_empty() => {
            println!("{}", build(&root)?.display());
        }
        Some((action, rest)) if action == "build" => build_cli(&root, rest, || build(&root))?,
        Some((action, rest)) if action == "clean-assets" && rest.is_empty() => clean_assets(&root)?,
        Some((action, rest)) if action == "--help" && rest.is_empty() => println!("{usage}"),
        _ => return Err(usage.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_assets_keep_the_publication_and_its_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let write = |out: &Path, time| -> Result<()> {
            fs::write(out.join("kernel"), b"kernel")?;
            assets::tests::archive(&out.join("driver-runtime.tar.gz"), b"init", 0o755, time);
            Ok(())
        };
        let first = publish(root.path(), |out| write(out, 1)).unwrap();
        let pointer = root.path().join("build/assets");
        let modified = pointer.symlink_metadata().unwrap().modified().unwrap();
        let archive = fs::read(first.join("driver-runtime.tar.gz")).unwrap();
        let second = publish(root.path(), |out| write(out, 2)).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            pointer.symlink_metadata().unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(
            fs::read(first.join("driver-runtime.tar.gz")).unwrap(),
            archive
        );
        assert_eq!(fs::read_dir(root.path().join("build")).unwrap().count(), 3);
    }

    #[test]
    fn added_removed_and_changed_files_publish_new_generations() {
        let root = tempfile::tempdir().unwrap();
        let mut previous = PathBuf::new();
        for files in [
            vec![("kernel", "a")],
            vec![("kernel", "a"), ("extra", "b")],
            vec![("kernel", "a")],
            vec![("kernel", "b")],
        ] {
            let next = publish(root.path(), |out| {
                for (name, bytes) in files {
                    fs::write(out.join(name), bytes)?;
                }
                Ok(())
            })
            .unwrap();
            assert_ne!(next, previous);
            previous = next;
        }
    }

    #[test]
    fn cleanup_preserves_current_generation_unrelated_directories_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let first = publish(root.path(), |out| Ok(fs::write(out.join("kernel"), b"a")?)).unwrap();
        let current = publish(root.path(), |out| Ok(fs::write(out.join("kernel"), b"b")?)).unwrap();
        let unrelated = root.path().join("build/other");
        fs::create_dir(&unrelated).unwrap();
        let external = tempfile::tempdir().unwrap();
        let link = root.path().join("build/.assets-external");
        symlink(external.path(), &link).unwrap();
        clean_assets(root.path()).unwrap();
        assert!(!first.exists());
        assert!(current.exists());
        assert!(unrelated.exists());
        assert!(link.symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            root.path().join("build/assets").canonicalize().unwrap(),
            current
        );
    }

    #[test]
    fn failed_preparation_never_reaches_cargo() {
        let root = tempfile::tempdir().unwrap();
        let error = build_cli(root.path(), &[], || Err("preparation failed".into())).unwrap_err();
        assert_eq!(error.to_string(), "preparation failed");
    }

    #[test]
    fn build_prepares_before_cargo_and_forwards_build_options() {
        let root = tempfile::tempdir().unwrap();
        let _guard = seedbox_test_time::HangGuard::new(
            "xtask Cargo build fixture",
            std::time::Duration::from_secs(60),
        );
        build_cli(
            root.path(),
            &["--release".into(), "--target-dir".into(), "output".into(), "--offline".into()],
            || {
                // Cargo can only build the fixture after preparation creates it.
                fs::write(root.path().join("Cargo.toml"), "[package]\nname = 'asset-build-fixture'\nversion = '0.0.0'\nedition = '2024'\n")?;
                fs::create_dir(root.path().join("src"))?;
                fs::write(root.path().join("src/main.rs"), "fn main() {}")?;
                Ok(root.path().to_owned())
            },
        ).unwrap();
        assert!(root
            .path()
            .join("output/release/asset-build-fixture")
            .is_file());
        // Cargo errors must also propagate out of the wrapper.
        assert!(
            build_cli(root.path(), &["--invalid-build-option".into()], || Ok(root
                .path()
                .to_owned()))
            .is_err()
        );
    }

    #[test]
    fn failed_assets_preserve_previous_publication_and_success_replaces_it() {
        let root = tempfile::tempdir().unwrap();
        let first = publish(root.path(), |out| {
            fs::write(out.join("kernel"), b"old")?;
            Ok(())
        })
        .unwrap();
        assert!(publish(root.path(), |out| {
            fs::write(out.join("kernel"), b"partial")?;
            Err("builder failed".into())
        })
        .is_err());
        assert_eq!(
            root.path().join("build/assets").canonicalize().unwrap(),
            first.canonicalize().unwrap()
        );
        let second = publish(root.path(), |out| {
            fs::write(out.join("kernel"), b"new")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            root.path().join("build/assets").canonicalize().unwrap(),
            second.canonicalize().unwrap()
        );
        assert_eq!(fs::read(first.join("kernel")).unwrap(), b"old");
        assert_eq!(fs::read(second.join("kernel")).unwrap(), b"new");
    }
}
