//! Opt-in public-contract fixtures for this crate's integration tests and examples.
// Feature-gated rather than cfg(test): integration tests are separate crates,
// and cfg(test) items are invisible to them.
use crate::driver::InputKey;
use seedbox_machine::definition::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod fixture;
pub mod native;

pub use crate::driver::stops::StopPolicy;

/// A key unique within this test process, so it is never live twice.
pub fn auto_key() -> InputKey {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    InputKey::new(format!("auto-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap()
}

/// A network floor of one quantum, at most the longest floor (an hour), and
/// nothing else. What follows from an output is due one floor after it, and
/// every runnable machine bounds a round by it, so fixtures whose scripts
/// are timed in nanoseconds use this instead of the default millisecond:
/// a busy round still ends one quantum after the earliest machine's start,
/// and consequences come one quantum after their output.
pub fn floor(quantum: std::time::Duration) -> crate::driver::NetworkConfig {
    let latency = quantum.min(std::time::Duration::from_secs(3600));
    crate::driver::NetworkConfig::new(latency, std::time::Duration::ZERO, 0).unwrap()
}

/// A limited playback rate too fast ever to make a host wait. Playback is
/// paced, so every round ends at the next quantum boundary at the latest, as
/// paced playback and single stepping need, without host time in a test.
pub fn paced() -> crate::driver::PlaybackRate {
    let one = std::num::NonZeroU32::MIN;
    crate::driver::PlaybackRate::Limited {
        numerator: std::num::NonZeroU32::MAX,
        denominator: one,
    }
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

/// The spec fixture spawns request: a host prepares any spec, so its text
/// only has to match between the spawn and its definition.
pub const SPAWN_SPEC: &str = "{}";

/// `definition` as the host's answer to a spawn of [`SPAWN_SPEC`].
pub fn spawn_definition(definition: MachineDefinition) -> crate::driver::SpawnDefinition {
    crate::driver::SpawnDefinition {
        spec: SPAWN_SPEC.into(),
        definition,
        allow_crashes: false,
    }
}
