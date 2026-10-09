//! Default locations for runtime inputs, so a run needs only a Compose file.
//!
//! Each resolver takes the explicit flag (clap has already applied its
//! `SEEDBOX_*` environment variable) and otherwise searches conventional
//! locations. A failed search names every place it looked and how to fix it.

// Linux input resolvers remain covered by portable tests; native Mac inputs
// come from the selected guest asset archive.
#![cfg_attr(all(target_os = "macos", target_arch = "aarch64"), allow(dead_code))]

use seedbox::{
    runtime_assets::{AssetSelection, RuntimeAssets},
    store::ContentStore,
    Error, Result,
};
use std::{
    env,
    path::{Path, PathBuf},
};

#[cfg(test)]
#[path = "../../tests/support/linux_assets.rs"]
mod linux_asset_fixtures;

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

/// Caller-selected locations precede the package; checkout and home searches
/// remain fallbacks for development binaries without a packaged runtime.
struct KernelLocations {
    configured: Vec<PathBuf>,
    incidental: Vec<PathBuf>,
}
impl KernelLocations {
    fn from_environment() -> Self {
        let mut configured = Vec::new();
        if let Some(artifacts) = env::var_os("SEEDBOX_ARTIFACTS") {
            configured.push(PathBuf::from(artifacts).join("bzImage"));
        }
        if let Some(work) = env::var_os("SEEDBOX_WORK") {
            configured.push(PathBuf::from(work).join("artifacts/bzImage"));
        }
        let mut incidental = vec![PathBuf::from("artifacts/bzImage")];
        for home in [env::var_os("HOME").map(PathBuf::from), sudo_home()]
            .into_iter()
            .flatten()
        {
            let kernel = home.join("seedbox-sim/artifacts/bzImage");
            if !incidental.contains(&kernel) {
                incidental.push(kernel);
            }
        }
        Self {
            configured,
            incidental,
        }
    }
    fn resolve(&self, explicit: Option<PathBuf>, packaged: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(path) = explicit {
            return existing("--kernel", path);
        }
        let mut searched = self.configured.clone();
        searched.extend(packaged);
        searched.extend(self.incidental.iter().cloned());
        first_file(&searched).ok_or_else(|| missing(
            "guest kernel", &searched,
            "install a packaged seedbox, build one with `bash kernel/build.sh`, or pass --kernel or set SEEDBOX_KERNEL",
        ))
    }
}

/// Legacy development resolver; the shared Linux resolver also supplies the
/// packaged fallback while retaining its extraction owner.
#[cfg(test)]
fn kernel(explicit: Option<PathBuf>) -> Result<PathBuf> {
    KernelLocations::from_environment().resolve(explicit, None)
}

#[derive(Default)]
pub struct LinuxOverrides {
    pub kernel: Option<PathBuf>,
    pub init: Option<PathBuf>,
    pub busybox: Option<PathBuf>,
    pub mke2fs: Option<PathBuf>,
}
/// Keep `assets` alive while any of these package paths are used. The doctor
/// selects only the kernel and never needs a store or preparation inputs.
pub struct LinuxRuntime {
    pub kernel: PathBuf,
    pub init: Option<PathBuf>,
    pub busybox: Option<PathBuf>,
    pub mke2fs: Option<PathBuf>,
    pub assets: Option<RuntimeAssets>,
}
fn selected_input(
    explicit: Option<PathBuf>,
    packaged: Option<PathBuf>,
    flag: &str,
) -> Result<Option<PathBuf>> {
    explicit
        .or(packaged)
        .map(|path| existing(flag, path))
        .transpose()
}

/// Sole Linux CLI resolution boundary for doctor, prepare, run and explore.
pub fn linux_runtime(
    overrides: LinuxOverrides,
    parent: &Path,
    selection: AssetSelection,
    store: Option<&ContentStore>,
    cancellation: &seedbox::execution::CancellationToken,
) -> Result<LinuxRuntime> {
    if crate::cli::backends::released_assets() {
        let all = matches!(selection, AssetSelection::All);
        let kernel = crate::cli::backends::guest_kernel(overrides.kernel, cancellation, all)?;
        let runtime = if all {
            Some(crate::cli::backends::container_runtime(None, cancellation)?)
        } else {
            None
        };
        let resolve = |flag, explicit: Option<PathBuf>, name| match explicit {
            Some(path) => existing(flag, path),
            None => runtime
                .as_ref()
                .map(|runtime| runtime.input(name))
                .ok_or_else(|| Error::Integrity("missing preparation helpers".into())),
        };
        return Ok(LinuxRuntime {
            kernel,
            init: if all {
                Some(resolve("--guest-init", overrides.init, "compose-init")?)
            } else {
                None
            },
            busybox: if all {
                Some(resolve("--busybox", overrides.busybox, "busybox")?)
            } else {
                None
            },
            mke2fs: if all {
                Some(resolve("--mke2fs", overrides.mke2fs, "mke2fs")?)
            } else {
                None
            },
            assets: None,
        });
    }
    resolve_linux_runtime(
        overrides,
        parent,
        selection,
        store,
        KernelLocations::from_environment(),
        RuntimeAssets::embedded,
    )
}
fn resolve_linux_runtime(
    overrides: LinuxOverrides,
    parent: &Path,
    selection: AssetSelection,
    store: Option<&ContentStore>,
    locations: KernelLocations,
    load_assets: impl FnOnce(&Path, AssetSelection) -> Result<Option<RuntimeAssets>>,
) -> Result<LinuxRuntime> {
    // Check all caller overrides before opening or validating a package.
    for (flag, path) in [
        ("--kernel", &overrides.kernel),
        ("--guest-init", &overrides.init),
        ("--busybox", &overrides.busybox),
        ("--mke2fs", &overrides.mke2fs),
    ] {
        if let Some(path) = path {
            existing(flag, path.clone())?;
        }
    }
    let kernel_needs_package =
        overrides.kernel.is_none() && first_file(&locations.configured).is_none();
    let all = matches!(selection, AssetSelection::All);
    let needs_package = kernel_needs_package
        || (all
            && (overrides.init.is_none()
                || overrides.busybox.is_none()
                || overrides.mke2fs.is_none()));
    let assets = if needs_package {
        load_assets(parent, selection)?
    } else {
        None
    };
    let input = |name| assets.as_ref().map(|assets| assets.input(name));
    let kernel = locations.resolve(overrides.kernel, input("bzImage"))?;
    let (init, busybox, mke2fs) = if all {
        let store =
            store.ok_or_else(|| Error::Setup("Linux preparation needs a content store".into()))?;
        let init = selected_input(overrides.init, input("compose-init"), "--guest-init")?
            .map(Ok)
            .unwrap_or_else(|| guest_init(None, store, parent))?;
        let busybox = selected_input(overrides.busybox, input("busybox"), "--busybox")?
            .map(Ok)
            .unwrap_or_else(|| busybox(None))?;
        let mke2fs = match selected_input(overrides.mke2fs, input("mke2fs"), "--mke2fs")? {
            Some(path) => Some(path),
            None => mke2fs(None)?,
        };
        (Some(init), Some(busybox), mke2fs)
    } else {
        (None, None, None)
    };
    Ok(LinuxRuntime {
        kernel,
        init,
        busybox,
        mke2fs,
        assets,
    })
}

/// The home directory of the user who invoked sudo, if any.
pub(crate) fn sudo_home() -> Option<PathBuf> {
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

/// The VMM is linked into this executable. An explicit path selects the legacy
/// standalone worker contract for development and historical runs.
pub fn engine(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return existing("--vmm", path);
    }
    env::current_exe().map_err(|error| {
        Error::Setup(format!(
            "cannot locate the built-in VMM executable: {error}; reinstall seedbox or pass --vmm"
        ))
    })
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

/// Resolve a preparation executable at the CLI environment boundary.
pub fn preparation_tool(path: &Path) -> Result<PathBuf> {
    let candidate = if path.components().count() == 1 {
        path.to_str().and_then(on_path)
    } else {
        Some(path.to_owned())
    };
    candidate.filter(|p| p.is_file()).ok_or_else(|| {
        Error::Setup(format!(
            "{} not found; install it or enter `nix develop`",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_kernel_precedence_keeps_configured_paths_ahead_of_package() {
        let directory = tempfile::tempdir().unwrap();
        let explicit = directory.path().join("explicit");
        let configured = directory.path().join("configured");
        let packaged = directory.path().join("packaged");
        let incidental = directory.path().join("incidental");
        for path in [&explicit, &configured, &packaged, &incidental] {
            std::fs::write(path, b"kernel").unwrap();
        }
        let locations = KernelLocations {
            configured: vec![
                directory.path().join("missing-configured"),
                configured.clone(),
            ],
            incidental: vec![incidental.clone()],
        };
        assert_eq!(
            locations
                .resolve(Some(explicit.clone()), Some(packaged.clone()))
                .unwrap(),
            explicit
        );
        assert_eq!(
            locations.resolve(None, Some(packaged.clone())).unwrap(),
            configured
        );
        std::fs::remove_file(&configured).unwrap();
        assert_eq!(
            locations.resolve(None, Some(packaged.clone())).unwrap(),
            packaged
        );
        std::fs::remove_file(&packaged).unwrap();
        assert_eq!(locations.resolve(None, Some(packaged)).unwrap(), incidental);
        assert_eq!(
            locations
                .resolve(
                    Some(directory.path().join("missing-explicit")),
                    Some(incidental)
                )
                .unwrap_err()
                .to_string(),
            format!(
                "--kernel {}: not found",
                directory.path().join("missing-explicit").display()
            )
        );
    }

    #[test]
    fn linux_helper_overrides_precede_injected_packaged_inputs() {
        let explicit = tempfile::NamedTempFile::new().unwrap();
        let packaged = tempfile::NamedTempFile::new().unwrap();
        for flag in ["--guest-init", "--busybox", "--mke2fs"] {
            assert_eq!(
                selected_input(
                    Some(explicit.path().into()),
                    Some(packaged.path().into()),
                    flag
                )
                .unwrap(),
                Some(explicit.path().into())
            );
            assert_eq!(
                selected_input(None, Some(packaged.path().into()), flag).unwrap(),
                Some(packaged.path().into())
            );
            let missing = packaged.path().with_extension("missing");
            assert_eq!(
                selected_input(Some(missing.clone()), Some(packaged.path().into()), flag)
                    .unwrap_err()
                    .to_string(),
                format!("{flag} {}: not found", missing.display())
            );
            assert!(selected_input(None, None, flag).unwrap().is_none());
        }
    }

    #[test]
    fn linux_all_explicit_inputs_skip_package_installation() {
        let directory = tempfile::tempdir().unwrap();
        let input = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let path = Some(input.path().to_owned());
        let store = ContentStore::new(directory.path().join("objects"));
        let resolved = resolve_linux_runtime(
            LinuxOverrides {
                kernel: path.clone(),
                init: path.clone(),
                busybox: path.clone(),
                mke2fs: path.clone(),
            },
            directory.path(),
            AssetSelection::All,
            Some(&store),
            KernelLocations {
                configured: Vec::new(),
                incidental: Vec::new(),
            },
            |_, _| panic!("explicit inputs must not install a package"),
        )
        .unwrap();
        assert_eq!(resolved.kernel, input.path());
        assert_eq!(resolved.init, path);
        assert_eq!(resolved.busybox, path);
        assert_eq!(resolved.mke2fs, path);
        assert!(resolved.assets.is_none());
        assert!(!directory.path().join("objects").exists());
    }

    #[test]
    fn linux_doctor_resolves_only_kernel_without_store_or_helpers() {
        let directory = tempfile::tempdir().unwrap();
        let kernel = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let resolved = resolve_linux_runtime(
            LinuxOverrides::default(),
            directory.path(),
            AssetSelection::Kernel,
            None,
            KernelLocations {
                configured: vec![kernel.path().into()],
                incidental: Vec::new(),
            },
            |_, _| panic!("configured kernel must not install a package"),
        )
        .unwrap();
        assert_eq!(resolved.kernel, kernel.path());
        assert!(resolved.init.is_none() && resolved.busybox.is_none() && resolved.mke2fs.is_none());
        assert!(resolved.assets.is_none());
    }

    #[test]
    fn linux_invalid_explicit_helper_fails_before_package_installation() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-busybox");
        let result = resolve_linux_runtime(
            LinuxOverrides {
                busybox: Some(missing.clone()),
                ..Default::default()
            },
            directory.path(),
            AssetSelection::All,
            None,
            KernelLocations {
                configured: Vec::new(),
                incidental: Vec::new(),
            },
            |_, _| panic!("bad explicit inputs must fail before package installation"),
        );
        assert_eq!(
            result.err().unwrap().to_string(),
            format!("--busybox {}: not found", missing.display())
        );
    }

    #[test]
    fn linux_mixed_overrides_keep_package_alive_through_preparer_retention() {
        let directory = tempfile::tempdir().unwrap();
        let kernel = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let init = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let store = ContentStore::new(directory.path().join("objects"));
        let bytes = linux_asset_fixtures::package();
        let resolved = resolve_linux_runtime(
            LinuxOverrides {
                kernel: Some(kernel.path().into()),
                init: Some(init.path().into()),
                ..Default::default()
            },
            directory.path(),
            AssetSelection::All,
            Some(&store),
            KernelLocations {
                configured: Vec::new(),
                incidental: Vec::new(),
            },
            |parent, selection| {
                RuntimeAssets::install(&bytes, parent, "x86_64-linux", selection).map(Some)
            },
        )
        .unwrap();
        assert_eq!(resolved.kernel, kernel.path());
        assert_eq!(resolved.init.as_deref(), Some(init.path()));
        let assets = resolved.assets.unwrap();
        assert_eq!(resolved.busybox, Some(assets.input("busybox")));
        assert_eq!(resolved.mke2fs, Some(assets.input("mke2fs")));
        let root = assets.input("bzImage").parent().unwrap().to_owned();
        assert!(!directory.path().join("objects").exists());
        let preparer = super::super::backends::Preparer::Linux(Box::new(
            super::super::backends::LinuxPreparer {
                inputs: seedbox::preparation::OciPreparer {
                    kernel: resolved.kernel,
                    engine: kernel.path().into(),
                    init: resolved.init.unwrap(),
                    busybox: resolved.busybox.unwrap(),
                    mke2fs: resolved.mke2fs,
                    tools: seedbox::oci::OciTools::default(),
                    profile: seedbox::execution::ExecutionProfile {
                        backend: "portable-fixture".into(),
                        engine: seedbox::store::ContentDigest::of(b"engine"),
                        witness_mode: seedbox::execution::WitnessMode::Exits,
                        semantic_parameters: Default::default(),
                    },
                },
                _assets: Some(assets),
            },
        ));
        let digest = store.put_file(&preparer.busybox).unwrap();
        assert!(root.is_dir());
        assert!(preparer.mke2fs.as_ref().unwrap().is_file());
        drop(preparer);
        assert!(!root.exists());
        assert_eq!(store.read(&digest).unwrap(), linux_asset_fixtures::elf());
    }

    #[test]
    fn linux_doctor_package_is_kernel_only_and_dropped_after_probe_scope() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = linux_asset_fixtures::package();
        let runtime = resolve_linux_runtime(
            LinuxOverrides::default(),
            directory.path(),
            AssetSelection::Kernel,
            None,
            KernelLocations {
                configured: Vec::new(),
                incidental: Vec::new(),
            },
            |parent, selection| {
                assert_eq!(selection, AssetSelection::Kernel);
                RuntimeAssets::install(&bytes, parent, "x86_64-linux", selection).map(Some)
            },
        )
        .unwrap();
        let root = runtime.kernel.parent().unwrap().to_owned();
        assert!(runtime.kernel.is_file());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        assert!(runtime.init.is_none() && runtime.busybox.is_none() && runtime.mke2fs.is_none());
        drop(runtime);
        assert!(!root.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

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
        assert_eq!(mke2fs(Some(path.clone())).unwrap(), Some(path.clone()));
        let error = kernel(Some("/nonexistent/bzImage".into())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "--kernel /nonexistent/bzImage: not found"
        );
    }

    #[test]
    fn engine_defaults_to_the_current_executable() {
        assert_eq!(engine(None).unwrap(), env::current_exe().unwrap());
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
