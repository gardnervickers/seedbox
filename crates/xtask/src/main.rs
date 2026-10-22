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
/// Like `Command::output` with stdin null and stdout captured; stderr is
/// whatever the caller set (std's default would capture it).
fn guarded_output(command: &mut Command) -> io::Result<std::process::Output> {
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    seedbox_exec::spawn(command)?.wait_with_output()
}

fn run(command: &mut Command) -> Result<()> {
    let status = seedbox_exec::status(
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
fn checkout_files(root: &Path) -> Result<Vec<u8>> {
    // Expand gitlinks into tracked files. Passing a submodule directory to tar
    // would also copy its .git pointer and ignored local build output.
    let mut files = Vec::new();
    for options in [
        vec!["--cached", "--recurse-submodules"],
        vec!["--others", "--exclude-standard"],
    ] {
        let listed = guarded_output(
            command("git", root)
                .arg("ls-files")
                .args(options)
                .arg("-z")
                .stderr(Stdio::piped()),
        )?;
        if !listed.status.success() {
            return Err("cannot list checkout files".into());
        }
        for name in listed.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let path = root.join(std::ffi::OsStr::from_bytes(name));
            if fs::symlink_metadata(path).is_ok_and(|m| m.is_file() || m.file_type().is_symlink()) {
                files.extend_from_slice(name);
                files.push(0);
            }
        }
    }
    Ok(files)
}
fn archive_checkout(root: &Path, directory: &Path) -> Result<PathBuf> {
    let archive = directory.join("source.tar");
    fs::write(directory.join("files"), checkout_files(root)?)?;
    run(command("tar", root)
        .args(["--no-recursion", "--null", "-T"])
        .arg(directory.join("files"))
        .arg("-cf")
        .arg(&archive)
        .env("COPYFILE_DISABLE", "1"))?;
    Ok(archive)
}
fn mac(root: &Path, output: &Path) -> Result<()> {
    let candidates = env::var("SEEDBOX_ASSET_BUILDER")
        .map(|s| vec![s])
        .unwrap_or_else(|_| vec!["podman".into(), "docker".into()]);
    let client = candidates
        .into_iter()
        .find(|p| {
            seedbox_exec::status(
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
    let archive = archive_checkout(root, source.path())?;
    let script = r#"mkdir /source /output
 tar -xf - -C /source
 cd /source
 export NIX_CONFIG="experimental-features = nix-command flakes
sandbox = false"
 inputs=$(nix build --cores 2 --max-jobs 1 --impure --no-link --print-out-paths --file nix/build-inputs.nix --argstr flake path:/source --arg names '[ "machinePvAssets" "machineTcgAssets" "machineGuestFormatter" "machineGuestFormatterX86" ]')
 pv=$inputs/machinePvAssets
 tcg=$inputs/machineTcgAssets
 formatter=$inputs/machineGuestFormatter
 formatter_x86=$inputs/machineGuestFormatterX86
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
            .stdin(fs::File::open(archive)?))?;
        run(command(&client, root)
            .args(["cp", &format!("{container}:/output/.")])
            .arg(output))
    })();
    let cleanup = run(command(&client, root).args(["rm", "--force", &container]));
    result.and(cleanup)
}
fn build(root: &Path) -> Result<PathBuf> {
    run(command("bash", root).arg(root.join("tools/bootstrap-submodules.sh")))?;
    publish(root, |output| {
        let mut names = vec!["tcgQemu"];
        if cfg!(target_os = "linux") {
            names.extend([
                "machinePvAssets",
                "machineTcgAssets",
                "machineGuestFormatter",
                "machineGuestFormatterX86",
            ]);
        }
        let inputs = runtime::nix_outputs(root, &names)?;
        match (env::consts::OS, env::consts::ARCH) {
            ("linux", "x86_64" | "aarch64") => {
                runtime::guest(&inputs, &output.join("driver"), env::consts::ARCH)?
            }
            ("macos", "aarch64") => mac(root, output)?,
            _ => return Err("requires Linux x86-64/AArch64 or Apple Silicon macOS".into()),
        }
        let driver = output.join("driver");
        runtime::host(&inputs["tcgQemu"], &driver)?;
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
    let assets = prepare()?;
    run(command("cargo", root).arg("build").args(args).env(
        "SEEDBOX_BUILD_DRIVER_RUNTIME",
        assets.join("driver-runtime.tar.gz"),
    ))
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
    fn checkout_archive_includes_submodule_edits_without_metadata_or_build_output() {
        let root = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let dr = root.path().join("vendor/dynamorio");
        fs::create_dir_all(&dr).unwrap();
        let git = |path: &Path, args: &[&str]| capture(command("git", path).args(args)).unwrap();
        // Local fixture repositories only; no remote or developer identity.
        git(root.path(), &["init", "--quiet"]);
        git(&dr, &["init", "--quiet"]);
        fs::write(dr.join("CMakeLists.txt"), b"original").unwrap();
        fs::write(dr.join(".gitignore"), b"build/\n").unwrap();
        git(&dr, &["add", "."]);
        git(
            &dr,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        fs::write(root.path().join(".gitmodules"), b"[submodule \"vendor/dynamorio\"]\npath = vendor/dynamorio\nurl = https://example.invalid/dynamorio.git\n").unwrap();
        git(root.path(), &["add", ".gitmodules", "vendor/dynamorio"]);
        git(root.path(), &["submodule", "init"]);
        git(root.path(), &["submodule", "absorbgitdirs"]);
        fs::write(dr.join("CMakeLists.txt"), b"local edit").unwrap();
        fs::write(dr.join("new.c"), b"new tracked source").unwrap();
        git(&dr, &["add", "new.c"]);
        fs::create_dir(dr.join("build")).unwrap();
        fs::write(dr.join("build/ignored.o"), b"ignored").unwrap();
        symlink("CMakeLists.txt", dr.join("source-link")).unwrap();
        git(&dr, &["add", "source-link"]);
        let archive = archive_checkout(root.path(), output.path()).unwrap();
        let extracted = output.path().join("extracted");
        tar::Archive::new(fs::File::open(archive).unwrap())
            .unpack(&extracted)
            .unwrap();
        let exported = extracted.join("vendor/dynamorio");
        assert_eq!(
            fs::read(exported.join("CMakeLists.txt")).unwrap(),
            b"local edit"
        );
        assert_eq!(
            fs::read(exported.join("new.c")).unwrap(),
            b"new tracked source"
        );
        assert_eq!(
            fs::read_link(exported.join("source-link")).unwrap(),
            Path::new("CMakeLists.txt")
        );
        assert!(!exported.join(".git").exists());
        assert!(!exported.join("build").exists());
        assert!(!extracted.join(".git").exists());
    }

    #[test]
    fn failed_bootstrap_stops_before_asset_building() {
        let root = tempfile::tempdir().unwrap();
        let error = build(root.path()).unwrap_err().to_string();
        assert!(error.contains("bootstrap-submodules.sh"));
        assert!(!root.path().join("build").exists());
    }

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
                fs::write(root.path().join("build.rs"), "fn main() { let path = std::env::var_os(\"SEEDBOX_BUILD_DRIVER_RUNTIME\").expect(\"explicit runtime request\"); assert!(std::path::Path::new(&path).ends_with(\"driver-runtime.tar.gz\")); }")?;
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
