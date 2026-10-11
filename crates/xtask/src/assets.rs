use std::{
    env, fs, io,
    os::unix::{ffi::OsStrExt, fs::symlink, io::AsRawFd},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .stdout(Stdio::from(io::stderr()))
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(format!("{command:?}: {status}").into());
    }
    Ok(())
}
fn capture(command: &mut Command) -> Result<String> {
    let output = command.stderr(Stdio::inherit()).output()?;
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
    Ok(())
}
fn mac(root: &Path, output: &Path) -> Result<()> {
    let candidates = env::var("SEEDBOX_ASSET_BUILDER")
        .map(|s| vec![s])
        .unwrap_or_else(|_| vec!["podman".into(), "docker".into()]);
    let client = candidates
        .into_iter()
        .find(|p| {
            Command::new(p)
                .arg("info")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
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
    let listed = command("git", root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()?;
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
pub fn build(root: &Path) -> Result<PathBuf> {
    publish(root, |output| match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64" | "aarch64") => linux(root, output),
        ("macos", "aarch64") => mac(root, output),
        _ => Err("requires Linux x86-64/AArch64 or Apple Silicon macOS".into()),
    })
}

fn publish(root: &Path, build_assets: impl FnOnce(&Path) -> Result<()>) -> Result<PathBuf> {
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
    let pending = tempfile::Builder::new()
        .prefix(".assets-")
        .tempdir_in(&directory)?;
    build_assets(pending.path())?;
    fs::write(pending.path().join("architecture"), env::consts::ARCH)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
