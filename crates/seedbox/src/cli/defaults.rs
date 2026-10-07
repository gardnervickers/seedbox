//! Default locations for runtime inputs, so a run needs only a Compose file.
//!
//! Each resolver takes the explicit flag (clap has already applied its
//! `SEEDBOX_*` environment variable) and otherwise searches conventional
//! locations. A failed search names every place it looked and how to fix it.

use seedbox::{store::ContentStore, Error, Result};
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// A Compose file path, or the Compose file inside a project directory.
pub fn compose_file(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return existing("Compose file", path.to_owned());
    }
    COMPOSE_FILES
        .iter()
        .map(|name| path.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            let searched: Vec<_> = COMPOSE_FILES.iter().map(|name| path.join(name)).collect();
            missing("Compose file", &searched, "pass the file path directly")
        })
}

/// The guest kernel: `$SEEDBOX_ARTIFACTS/bzImage` or
/// `$SEEDBOX_WORK/artifacts/bzImage`, as written by `kernel/build.sh`, then
/// `./artifacts/bzImage`, then the build script's default work directory in
/// the home of the current user or the user who invoked sudo.
pub fn kernel(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return existing("--kernel", path);
    }
    let mut searched = Vec::new();
    if let Some(artifacts) = env::var_os("SEEDBOX_ARTIFACTS") {
        searched.push(PathBuf::from(artifacts));
    }
    if let Some(work) = env::var_os("SEEDBOX_WORK") {
        searched.push(PathBuf::from(work).join("artifacts"));
    }
    searched.push(PathBuf::from("artifacts"));
    // Under sudo, HOME is root's; the kernel was built in the caller's home.
    for home in [env::var_os("HOME").map(PathBuf::from), sudo_home()]
        .into_iter()
        .flatten()
    {
        let artifacts = home.join("seedbox-sim/artifacts");
        if !searched.contains(&artifacts) {
            searched.push(artifacts);
        }
    }
    let searched: Vec<_> = searched.iter().map(|dir| dir.join("bzImage")).collect();
    first_file(&searched).ok_or_else(|| {
        missing(
            "guest kernel",
            &searched,
            "build one with `bash kernel/build.sh`, or pass --kernel or set SEEDBOX_KERNEL",
        )
    })
}

/// The home directory of the user who invoked sudo, if any.
fn sudo_home() -> Option<PathBuf> {
    use std::{ffi::CStr, ffi::CString, os::unix::ffi::OsStrExt};
    let user = CString::new(env::var_os("SUDO_USER")?.as_bytes()).ok()?;
    // getpwnam is not thread-safe; the CLI calls this before starting threads.
    let entry = unsafe { libc::getpwnam(user.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    let home = unsafe { CStr::from_ptr((*entry).pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())))
}

/// The VMM: `seedbox-vmm` next to this executable (an installed pair), then
/// this checkout's `cargo build-vmm` output, then on `PATH`. Under `cargo run`
/// in this checkout, a missing VMM is built exactly as `cargo build-vmm` does.
pub fn engine(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return existing("--vmm", path);
    }
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent();
    let vmm = crates.unwrap_or(Path::new(".")).join("seedbox-vmm");
    let built = vmm.join("target/release/seedbox-vmm");
    // Without a known executable path, only that one candidate is skipped.
    let mut searched = engine_candidates(env::current_exe().ok().as_deref(), &built);
    if let Some(path) = first_file(&searched).or_else(|| on_path("seedbox-vmm")) {
        return Ok(path);
    }
    if let Some(cargo) = cargo_run_here() {
        build_engine(&cargo, &vmm)?;
        if built.is_file() {
            return Ok(built);
        }
    }
    searched.push(PathBuf::from("$PATH/seedbox-vmm"));
    Err(missing(
        "VMM",
        &searched,
        "build it with `cargo build-vmm`, or pass --vmm or set SEEDBOX_VMM",
    ))
}

/// Where to look for the VMM, in order: next to `exe`, then `built`. A cargo
/// profile directory (it holds `.cargo-lock`) never receives the VMM, which
/// builds in its own workspace, so a `seedbox-vmm` found there is stale.
fn engine_candidates(exe: Option<&Path>, built: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(directory) = exe
        .and_then(Path::parent)
        .filter(|dir| !dir.join(".cargo-lock").exists())
    {
        candidates.push(directory.join("seedbox-vmm"));
    }
    candidates.push(built.to_owned());
    candidates
}

/// The cargo running this binary, when it was started by `cargo run` (or a
/// test) in this checkout rather than by an unrelated cargo project.
fn cargo_run_here() -> Option<std::ffi::OsString> {
    let manifest = env::var_os("CARGO_MANIFEST_DIR")?;
    (Path::new(&manifest) == Path::new(env!("CARGO_MANIFEST_DIR")))
        .then(|| env::var_os("CARGO"))
        .flatten()
}

/// Build the VMM with the same arguments as the `cargo build-vmm` alias.
fn build_engine(cargo: &std::ffi::OsStr, vmm: &Path) -> Result<()> {
    eprintln!("seedbox-vmm not built; building it with `cargo build-vmm`");
    let status = Command::new(cargo)
        .args(["build", "--release", "--locked", "--manifest-path"])
        .arg(vmm.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(vmm.join("target"))
        .status()
        .map_err(|error| Error::Setup(format!("running cargo to build seedbox-vmm: {error}")))?;
    if !status.success() {
        return Err(Error::Setup(
            "building seedbox-vmm failed (see cargo output above)".into(),
        ));
    }
    Ok(())
}

/// The static guest supervisor: the copy embedded at build time, written to
/// `directory` through the content store.
pub fn guest_init(
    explicit: Option<PathBuf>,
    store: &ContentStore,
    directory: &Path,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return existing("--guest-init", path);
    }
    #[cfg(seedbox_embedded_init)]
    {
        const INIT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/compose-init"));
        let digest = store.put(INIT)?;
        store.materialize(&digest, directory)
    }
    #[cfg(not(seedbox_embedded_init))]
    {
        let _ = (store, directory);
        Err(Error::Setup(
            "no guest supervisor is built in (this seedbox was compiled without a musl \
             compiler); rebuild inside `nix develop` or with musl-gcc installed, or pass \
             --guest-init or set SEEDBOX_GUEST_INIT"
                .into(),
        ))
    }
}

/// A static Linux amd64 BusyBox: `busybox` on `PATH` when it is statically
/// linked (the Nix shell sets SEEDBOX_BUSYBOX instead).
pub fn busybox(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return existing("--busybox", path);
    }
    match on_path("busybox") {
        Some(path) if is_static_elf(&path) => Ok(path),
        Some(path) => Err(Error::Setup(format!(
            "{} is dynamically linked; install a static BusyBox, or pass --busybox or set \
             SEEDBOX_BUSYBOX",
            path.display()
        ))),
        None => Err(missing(
            "static BusyBox",
            &[PathBuf::from("$PATH/busybox")],
            "enter `nix develop`, or pass --busybox or set SEEDBOX_BUSYBOX",
        )),
    }
}

/// A static Linux amd64 mke2fs, if there is one: an explicit path, else
/// `mke2fs` on `PATH` when it is statically linked (the Nix shell sets
/// SEEDBOX_MKE2FS). Only Compose files with named volumes need it, so
/// preparation reports its absence then.
pub fn mke2fs(explicit: Option<PathBuf>) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        return existing("--mke2fs", path).map(Some);
    }
    Ok(on_path("mke2fs").filter(|path| is_static_elf(path)))
}

/// A recorded run: a directory holding `manifest.json`.
pub fn run_directory(path: PathBuf) -> Result<PathBuf> {
    let path = existing("run directory", path)?;
    if path.join("manifest.json").is_file() {
        Ok(path)
    } else {
        Err(Error::Setup(format!(
            "{}: not a run directory (no manifest.json)",
            path.display()
        )))
    }
}

/// An explicitly named input, checked before any slow preparation starts.
pub fn existing(what: &str, path: PathBuf) -> Result<PathBuf> {
    if path.exists() {
        Ok(path)
    } else {
        Err(Error::Setup(format!(
            "{what} {}: not found",
            path.display()
        )))
    }
}

fn missing(what: &str, searched: &[PathBuf], fix: &str) -> Error {
    let searched: Vec<_> = searched
        .iter()
        .map(|path| format!("  {}", path.display()))
        .collect();
    Error::Setup(format!(
        "no {what} found; searched:\n{}\n{fix}",
        searched.join("\n")
    ))
}

fn first_file(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|path| path.is_file()).cloned()
}

fn on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

/// A 64-bit little-endian ELF executable with no `PT_INTERP` program header.
fn is_static_elf(path: &Path) -> bool {
    const PT_INTERP: u64 = 3;
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let field = |at: usize, width: usize| -> Option<u64> {
        let bytes = bytes.get(at..at + width)?;
        Some(
            bytes
                .iter()
                .rev()
                .fold(0, |value, byte| value << 8 | *byte as u64),
        )
    };
    if bytes.get(..6) != Some(b"\x7fELF\x02\x01") {
        return false;
    }
    let (Some(table), Some(size), Some(count)) = (field(0x20, 8), field(0x36, 2), field(0x38, 2))
    else {
        return false;
    };
    (0..count as usize).all(|index| {
        let at = table as usize + index * size as usize;
        field(at, 4).is_some_and(|kind| kind != PT_INTERP)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_directory_resolves_to_its_compose_file() {
        let directory = tempfile::tempdir().unwrap();
        assert!(compose_file(directory.path()).is_err());
        std::fs::write(directory.path().join("docker-compose.yml"), "").unwrap();
        assert_eq!(
            compose_file(directory.path()).unwrap(),
            directory.path().join("docker-compose.yml")
        );
        std::fs::write(directory.path().join("compose.yaml"), "").unwrap();
        assert_eq!(
            compose_file(directory.path()).unwrap(),
            directory.path().join("compose.yaml")
        );
        let file = directory.path().join("other.yaml");
        assert!(compose_file(&file).is_err());
        std::fs::write(&file, "").unwrap();
        assert_eq!(compose_file(&file).unwrap(), file);
    }

    #[test]
    fn explicit_inputs_are_checked_but_never_searched() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        assert_eq!(kernel(Some(path.clone())).unwrap(), path);
        assert_eq!(engine(Some(path.clone())).unwrap(), path);
        assert_eq!(busybox(Some(path.clone())).unwrap(), path);
        let error = kernel(Some("/nonexistent/bzImage".into())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "--kernel /nonexistent/bzImage: not found"
        );
    }

    #[test]
    fn installed_vmm_wins_but_cargo_profile_dirs_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let built = directory.path().join("checkout/seedbox-vmm");
        let installed = directory.path().join("bin");
        std::fs::create_dir(&installed).unwrap();
        assert_eq!(
            engine_candidates(Some(&installed.join("seedbox")), &built),
            [installed.join("seedbox-vmm"), built.clone()]
        );
        let profile = directory.path().join("target/debug");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join(".cargo-lock"), "").unwrap();
        let only_built = std::slice::from_ref(&built);
        assert_eq!(
            engine_candidates(Some(&profile.join("seedbox")), &built),
            only_built
        );
        assert_eq!(engine_candidates(None, &built), only_built);
    }

    #[test]
    fn static_elf_detection() {
        assert!(!is_static_elf(Path::new("/nonexistent")));
        // The test binary itself is dynamically linked on glibc hosts.
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert!(!is_static_elf(&env::current_exe().unwrap()));
        }
    }

    #[cfg(seedbox_embedded_init)]
    #[test]
    fn embedded_guest_init_is_a_static_executable() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::new(directory.path().join("objects"));
        let path = guest_init(None, &store, &directory.path().join("bin")).unwrap();
        assert!(is_static_elf(&path));
    }
}
