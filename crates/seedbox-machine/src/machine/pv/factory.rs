use super::{PVMachine, boot};
use crate::{
    application::{GuestBootstrap, Kernel, boot_derived, with_runtime},
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
        assets::kernel(&kernel, true)?;
        let init = assets::read(guest_assets, "init")?;
        assets::elf(&init, 183, true)?;
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
        let prefix = [
            "/seedbox/progress/bin64/drrun",
            "-opt_speed",
            "-disable_traces",
            "-ldstex2cas",
            "-c",
            "/seedbox/progress/libseedbox_progress.so",
            "--blocks-per-progress-call",
            "32",
            "--",
        ]
        .map(str::to_owned);
        let derived = with_runtime(&definition.application, &self.runtime, &prefix)
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
fn validate_runtime(bytes: &[u8]) -> io::Result<()> {
    let required = [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        "lib/ld-linux-aarch64.so.1",
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
                if matches!(
                    path,
                    "seedbox/progress/bin64/drrun" | "seedbox/progress/lib/ld-linux-aarch64.so.1"
                ) && entry.header().mode()? & 0o111 == 0
                {
                    return Err(io::Error::other(format!(
                        "PV runtime executable lacks execute permission: {path}"
                    )));
                }
                if !path.ends_with(".txt") && !path.ends_with(".LIB") {
                    let mut content = vec![];
                    std::io::Read::read_to_end(&mut entry, &mut content)?;
                    super::super::assets::elf(&content, 183, false)?;
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
        let mut kernel = vec![0; 64];
        kernel[56..60].copy_from_slice(b"ARM\x64");
        kernel[16..24].copy_from_slice(&64_u64.to_le_bytes());
        fs::write(root.join("kernel"), kernel).unwrap();
        fs::write(root.join("init"), crate::machine::assets::tests::image(183)).unwrap();
        fs::write(root.join("command-line"), " console=x\n").unwrap();
        let files = [
            "bin64/drrun",
            "libseedbox_progress.so",
            "lib64/release/libdynamorio.so",
            "lib64/release/libdrpreload.so",
            "ext/lib64/release/libdrmgr.so",
            "ext/lib64/release/libdrreg.so",
            "lib/ld-linux-aarch64.so.1",
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
                crate::machine::assets::tests::image(183)
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
        for required in [
            b"seedbox/progress/bin64/drrun".as_slice(),
            b"-ldstex2cas",
            b"SBXAPP2",
        ] {
            assert!(
                first
                    .initrd
                    .windows(required.len())
                    .any(|bytes| bytes == required)
            );
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
        for broken in [
            "libdrpreload.so",
            "bin64/drrun",
            "lib/ld-linux-aarch64.so.1",
        ] {
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
