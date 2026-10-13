use super::{Error, TcgMachine};
use crate::{
    application::{GuestBootstrap, Kernel},
    definition::MachineDefinition,
};
use std::path::{Path, PathBuf};
/// Owns prepared guest inputs for constructing stopped TCG machines.
/// Loading performs no build, environment lookup or guest execution. The host
/// QEMU executable remains separate from the guest-only assets.
pub struct Factory {
    qemu: PathBuf,
    bootstrap: GuestBootstrap,
    firmware: Vec<u8>,
}
impl Factory {
    /// Load and validate the explicit offline-built guest asset directory once.
    /// Required files and supported structural formats are checked during load;
    /// runtime incompatibility can still fail when run. No alternate location is tried.
    pub fn new(qemu: &Path, guest_assets: &Path) -> Result<Self, Error> {
        use super::super::assets;
        assets::executable(qemu)?;
        let kernel = assets::read(guest_assets, "kernel")?;
        assets::kernel(&kernel, false)?;
        let init = assets::read(guest_assets, "init")?;
        assets::elf(&init, 243, true)?;
        let command_line = assets::arguments(guest_assets, 1024)?;
        let firmware = assets::read(guest_assets, "firmware")?;
        if firmware.starts_with(b"\x7fELF") {
            assets::elf(&firmware, 243, false)?;
        }
        Ok(Self {
            qemu: qemu.to_owned(),
            bootstrap: GuestBootstrap {
                kernel: Kernel {
                    image: kernel,
                    command_line,
                },
                init,
                base_initramfs: vec![],
            },
            firmware,
        })
    }
    /// Package the original application without binary rewriting.
    /// Construction consumes no clock or RNG and executes no guest instructions.
    /// Unsupported resources or unpackageable launch inputs are errors.
    pub fn create(&mut self, definition: &MachineDefinition) -> Result<TcgMachine, Error> {
        TcgMachine::new(
            &self.qemu,
            self.firmware.clone(),
            &self.bootstrap,
            definition.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn loaded_assets_are_owned_and_workload_is_reusable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut kernel = vec![0; 64];
        kernel[48..56].copy_from_slice(b"RISCV\0\0\0");
        kernel[56..60].copy_from_slice(b"RSC\x05");
        fs::write(root.join("kernel"), &kernel).unwrap();
        fs::write(root.join("init"), crate::machine::assets::tests::image(243)).unwrap();
        fs::write(root.join("command-line"), " custom=x\n").unwrap();
        fs::write(root.join("firmware"), [1]).unwrap();
        let mut factory = Factory::new(&std::env::current_exe().unwrap(), root).unwrap();
        let definition = crate::driver::fixture::definition(0);
        let original = definition.clone();
        let first = factory.create(&definition).unwrap();
        fs::remove_dir_all(root).unwrap();
        let second = factory.create(&definition).unwrap();
        assert_eq!(definition, original);
        match (&first.manifest.boot, &second.manifest.boot) {
            (
                super::super::Boot::Linux {
                    kernel: image,
                    command_line,
                    initramfs,
                    ..
                },
                super::super::Boot::Linux {
                    initramfs: again, ..
                },
            ) => {
                assert_eq!(image.as_ref(), kernel);
                assert_eq!(command_line, " custom=x\n");
                assert_eq!(initramfs, again);
            }
            _ => panic!("expected Linux"),
        }
        let mut unsupported = definition;
        unsupported.resources.memory_bytes = 1;
        assert!(factory.create(&unsupported).is_err());
    }
}
