use super::{boot, PVMachine};
use crate::{
    application::{boot_derived, with_runtime, GuestBootstrap, Kernel},
    definition::{FilesystemArchive, MachineDefinition},
};
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};
/// Owns prepared guest inputs for constructing stopped PV machines.
/// Loading performs no build, environment lookup or guest execution. The host
/// worker remains a separately supplied executable. Each created machine uses
/// automatic application instrumentation and owns its derived boot inputs.
pub struct Factory {
    worker: PathBuf,
    bootstrap: GuestBootstrap,
    runtime: FilesystemArchive,
}
impl Factory {
    /// Load and validate the explicit offline-built guest asset directory once.
    /// Required files and supported structural formats are checked during load;
    /// runtime incompatibility can still fail when run. No alternate location is tried.
    pub fn new(worker: &Path, guest_assets: &Path) -> io::Result<Self> {
        use super::super::assets;
        assets::executable(worker)?;
        let kernel = assets::read(guest_assets, "kernel")?;
        guest::kernel(&kernel)?;
        let init = assets::read(guest_assets, "init")?;
        assets::elf(&init, guest::ELF_MACHINE, true)?;
        let command_line = assets::arguments(guest_assets, 2048)?;
        let bytes = assets::read(guest_assets, "progress.tar")?;
        validate_runtime(&bytes)?;
        let runtime = FilesystemArchive::new(bytes).map_err(io::Error::other)?;
        Ok(Self {
            worker: worker.to_owned(),
            bootstrap: GuestBootstrap {
                kernel: Kernel {
                    image: kernel,
                    command_line,
                },
                init,
                base_initramfs: vec![],
            },
            runtime,
        })
    }
    /// Derive an instrumented application without changing the supplied workload.
    /// Construction consumes no clock or RNG and executes no guest instructions.
    /// Unsupported resources or unpackageable launch inputs are errors.
    pub fn create(&mut self, definition: &MachineDefinition) -> io::Result<PVMachine> {
        if definition.resources.memory_bytes != boot::RAM_BYTES as u64 {
            return Err(io::Error::other(
                "PV currently supports exactly 128 MiB RAM",
            ));
        }
        let derived = with_runtime(&definition.application, &self.runtime, &runtime_prefix())
            .map_err(io::Error::other)?;
        let initrd = boot_derived(&derived, &self.bootstrap).map_err(io::Error::other)?;
        PVMachine::construct(
            &self.worker,
            self.bootstrap.kernel.image.clone(),
            initrd,
            self.bootstrap.kernel.command_line.clone(),
        )
        .map_err(io::Error::other)
    }
}
/// Guest architecture details of the private kernel and instrumentation runtime.
#[cfg(target_arch = "aarch64")]
mod guest {
    pub(super) const ELF_MACHINE: u16 = 183;
    pub(super) const LOADER: &str = "lib/ld-linux-aarch64.so.1";
    pub(super) const LAUNCHER_OPTIONS: &[&str] = &[];
    /// ARM exclusives become compare-and-swap with software reservations.
    pub(super) const TRANSLATOR_OPTIONS: &[&str] = &["-ldstex2cas"];
    pub(super) fn kernel(bytes: &[u8]) -> std::io::Result<()> {
        super::super::super::assets::kernel(bytes, true)
    }
}
#[cfg(target_arch = "x86_64")]
mod guest {
    pub(super) const ELF_MACHINE: u16 = 62;
    pub(super) const LOADER: &str = "lib/ld-linux-x86-64.so.2";
    /// The x86 launcher otherwise warns on application stderr that the
    /// single-bitwidth runtime lacks 32-bit and debug cores. Launcher options
    /// precede the first translator option.
    pub(super) const LAUNCHER_OPTIONS: &[&str] = &["-quiet"];
    pub(super) const TRANSLATOR_OPTIONS: &[&str] = &[];
    /// A bzImage for the 64-bit boot protocol; the worker checks its layout.
    pub(super) fn kernel(bytes: &[u8]) -> std::io::Result<()> {
        if bytes.get(0x202..0x206) != Some(b"HdrS".as_slice()) {
            return Err(std::io::Error::other(
                "invalid or incompatible guest Linux bzImage",
            ));
        }
        Ok(())
    }
}
/// Mandatory translator launch prefix for every prepared application.
pub(super) fn runtime_prefix() -> Vec<String> {
    let mut prefix = vec!["/seedbox/progress/bin64/drrun"];
    prefix.extend_from_slice(guest::LAUNCHER_OPTIONS);
    prefix.extend(["-opt_speed", "-disable_traces"]);
    prefix.extend_from_slice(guest::TRANSLATOR_OPTIONS);
    prefix.extend([
        "-c",
        "/seedbox/progress/libseedbox_progress.so",
        "--blocks-per-progress-call",
        "32",
        "--",
    ]);
    prefix.into_iter().map(str::to_owned).collect()
}
fn validate_runtime(bytes: &[u8]) -> io::Result<()> {
    let required = [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        guest::LOADER,
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ];
    let mut missing: BTreeSet<_> = required
        .iter()
        .map(|name| format!("seedbox/progress/{name}"))
        .collect();
    let mut archive = tar::Archive::new(bytes);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if let Some(path) = path.to_str() {
            if missing.remove(path) {
                if !entry.header().entry_type().is_file() {
                    return Err(io::Error::other("PV runtime requires regular files"));
                }
                if (path == "seedbox/progress/bin64/drrun"
                    || path.strip_prefix("seedbox/progress/") == Some(guest::LOADER))
                    && entry.header().mode()? & 0o111 == 0
                {
                    return Err(io::Error::other(format!(
                        "PV runtime executable lacks execute permission: {path}"
                    )));
                }
                if !path.ends_with(".txt") && !path.ends_with(".LIB") {
                    let mut content = vec![];
                    std::io::Read::read_to_end(&mut entry, &mut content)?;
                    super::super::assets::elf(&content, guest::ELF_MACHINE, false)?;
                }
            }
        }
    }
    if let Some(path) = missing.first() {
        return Err(io::Error::other(format!(
            "PV instrumentation runtime missing {path}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn assets(root: &Path) {
        let kernel = super::super::test_image::program(&[super::super::test_image::Op::Spin]);
        fs::write(root.join("kernel"), kernel).unwrap();
        fs::write(
            root.join("init"),
            crate::machine::assets::tests::image(guest::ELF_MACHINE),
        )
        .unwrap();
        fs::write(root.join("command-line"), " console=x\n").unwrap();
        let files = [
            "bin64/drrun",
            "libseedbox_progress.so",
            "lib64/release/libdynamorio.so",
            "lib64/release/libdrpreload.so",
            "ext/lib64/release/libdrmgr.so",
            "ext/lib64/release/libdrreg.so",
            guest::LOADER,
            "lib/libc.so.6",
            "lib/libm.so.6",
            "lib/libdl.so.2",
            "lib/libpthread.so.0",
            "lib/librt.so.1",
            "DynamoRIO-LICENSE.txt",
            "DynamoRIO-ACKNOWLEDGEMENTS.txt",
            "glibc-COPYING.LIB",
        ];
        let mut archive = tar::Builder::new(vec![]);
        for name in files {
            let content = if name.ends_with(".txt") || name.ends_with(".LIB") {
                b"license".to_vec()
            } else {
                crate::machine::assets::tests::image(guest::ELF_MACHINE)
            };
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_size(content.len() as u64);
            header.set_cksum();
            archive
                .append_data(
                    &mut header,
                    format!("seedbox/progress/{name}"),
                    content.as_slice(),
                )
                .unwrap();
        }
        fs::write(root.join("progress.tar"), archive.into_inner().unwrap()).unwrap();
    }
    #[test]
    fn mandatory_runtime_derivation_owns_inputs_and_preserves_original_workload() {
        let directory = tempfile::tempdir().unwrap();
        assets(directory.path());
        let mut factory =
            Factory::new(&std::env::current_exe().unwrap(), directory.path()).unwrap();
        let definition = crate::driver::fixture::definition(0);
        let original = definition.clone();
        let first = factory.create(&definition).unwrap();
        fs::remove_dir_all(directory.path()).unwrap();
        let second = factory.create(&definition).unwrap();
        assert_eq!(definition, original);
        assert_eq!(first.initrd, second.initrd);
        assert_eq!(first.command_line, " console=x\n");
        for required in [b"seedbox/progress/bin64/drrun".as_slice(), b"SBXAPP2"]
            .into_iter()
            .chain(
                guest::LAUNCHER_OPTIONS
                    .iter()
                    .chain(guest::TRANSLATOR_OPTIONS)
                    .map(|option| option.as_bytes()),
            )
        {
            assert!(first
                .initrd
                .windows(required.len())
                .any(|bytes| bytes == required));
        }
        let mut unsupported = definition;
        unsupported.resources.memory_bytes = 1;
        assert!(factory.create(&unsupported).is_err());
    }
    #[test]
    fn missing_runtime_is_an_error_without_uninstrumented_fallback() {
        let directory = tempfile::tempdir().unwrap();
        assets(directory.path());
        fs::remove_file(directory.path().join("progress.tar")).unwrap();
        assert!(Factory::new(&std::env::current_exe().unwrap(), directory.path()).is_err());
    }
    #[test]
    fn runtime_preload_and_executable_modes_are_required_at_load() {
        for broken in ["libdrpreload.so", "bin64/drrun", guest::LOADER] {
            let directory = tempfile::tempdir().unwrap();
            assets(directory.path());
            let bytes = fs::read(directory.path().join("progress.tar")).unwrap();
            let mut source = tar::Archive::new(bytes.as_slice());
            let mut changed = tar::Builder::new(vec![]);
            for entry in source.entries().unwrap() {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().into_owned();
                if broken == "libdrpreload.so" && path.ends_with(broken) {
                    continue;
                }
                let mut header = entry.header().clone();
                if broken != "libdrpreload.so" && path.ends_with(broken) {
                    header.set_mode(0o644);
                    header.set_cksum();
                }
                changed.append_data(&mut header, &path, &mut entry).unwrap();
            }
            fs::write(
                directory.path().join("progress.tar"),
                changed.into_inner().unwrap(),
            )
            .unwrap();
            let error = match Factory::new(&std::env::current_exe().unwrap(), directory.path()) {
                Ok(_) => panic!("broken runtime accepted"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains(broken), "{error}");
            assert!(
                error.contains(if broken == "libdrpreload.so" {
                    "missing"
                } else {
                    "lacks execute permission"
                }),
                "{error}"
            );
        }
    }
}
