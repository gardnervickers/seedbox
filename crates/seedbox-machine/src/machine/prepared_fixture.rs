//! Private raw-boot fixtures; public native construction uses backend factories.
use crate::application::{GuestBootstrap, Kernel};
use std::{fs, path::PathBuf};
fn input(name: &str) -> PathBuf {
    std::env::var_os(name).expect(name).into()
}
pub(super) fn bootstrap(backend: &str) -> GuestBootstrap {
    GuestBootstrap {
        kernel: Kernel {
            image: fs::read(input(&format!("SEEDBOX_{backend}_KERNEL"))).unwrap(),
            command_line: if backend == "PV" && cfg!(target_arch = "aarch64") {
                "console=ttyAMA0 nosmp"
            } else {
                "console=ttyS0"
            }
            .into(),
        },
        init: fs::read(input(&format!("SEEDBOX_{backend}_INIT"))).unwrap(),
        base_initramfs: Vec::new(),
        epoch_seconds: 0,
    }
}
