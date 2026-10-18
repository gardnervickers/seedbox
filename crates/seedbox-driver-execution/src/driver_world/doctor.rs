//! Lookup-only Driver readiness. No preparation, guest execution or retained state.
use crate::{store::ContentDigest, Result};
use std::path::Path;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[path = "doctor/mac.rs"]
mod mac;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
type Observations = std::collections::BTreeMap<String, String>;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
type Observations = Vec<crate::admission::CoreObservation>;

#[derive(Debug, serde::Serialize)]
pub struct DoctorReport {
    pub engine: ContentDigest,
    pub kernel: ContentDigest,
    pub simulator_version: String,
    pub observations: Observations,
}

/// Probe only the native APIs used by Driver PV, with no executable launch.
/// The caller owns the selected SDK extraction and has validated its boot inputs.
pub fn doctor(engine: &Path, kernel: &Path) -> Result<DoctorReport> {
    let engine = crate::admission::engine_digest(engine)?;
    let kernel = crate::admission::file_digest(kernel)?;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let observations = mac::observe()?;
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    let observations = {
        if !super::platform::Host::current().runs_driver() {
            return Err(crate::Error::Setup(
                "Driver readiness requires Linux x86-64 or macOS aarch64".into(),
            ));
        }
        #[cfg(target_os = "linux")]
        crate::host::check_kvm_access()?;
        let mut observations = Vec::new();
        crate::admission::collect_admitted(&mut observations, crate::admission::observe)?;
        observations
    };
    Ok(DoctorReport {
        engine,
        kernel,
        simulator_version: crate::engine::SIMULATOR_VERSION.into(),
        observations,
    })
}
