//! Opt-in public-contract fixtures for this crate's integration tests and examples.
// Feature-gated rather than cfg(test): integration tests are separate crates,
// and cfg(test) items are invisible to them.
use crate::driver::InputKey;
use seedbox_machine::definition::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod fixture;
pub mod native;

/// A key unique within this test process, so it is never live twice.
pub fn auto_key() -> InputKey {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    InputKey::new(format!("auto-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap()
}

/// A fixed key for tests that correlate events with it.
pub fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}

/// Minimal configured inputs for public-contract fixture machines.
pub fn definition(name: impl Into<String>) -> MachineDefinition {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", &[][..])
        .unwrap();
    MachineDefinition {
        name: name.into(),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
            launch: Launch {
                argv: vec!["/application".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    }
}
