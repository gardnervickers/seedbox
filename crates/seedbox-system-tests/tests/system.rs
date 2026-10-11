//! These ignored cohorts need real kernel execution. `cargo xtask check` builds
//! this one binary, admits the requested suites, and supplies immutable inputs.
//! Each cohort owns its processes, files and sockets; only kernel admission is
//! shared. Module comments explain why a portable substitute is insufficient.
#[path = "system/support.rs"]
mod support;

#[path = "system/oci.rs"]
mod oci;

#[cfg(target_arch = "aarch64")]
#[path = "system/arm64.rs"]
mod arm64;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "system/x86.rs"]
mod x86;

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
#[path = "system/applications.rs"]
mod applications;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "system/pause.rs"]
mod pause;
