//! Held-before-effect capture through the public retained Driver service.
#![cfg(all(
    any(feature = "kvm-tests", feature = "tcg-tests"),
    target_os = "linux",
    target_arch = "x86_64"
))]
pub mod support {
    pub mod case_cleanup;
    pub mod native_service;
}
use support::native_service;

#[cfg(feature = "kvm-tests")]
#[tokio::test]
async fn pv_held_checkpoint_export_delete_import_releases_once() {
    native_service::native_held_checkpoint(native_service::CpuBackend::Pv).await;
}

#[cfg(feature = "tcg-tests")]
#[tokio::test]
async fn tcg_held_checkpoint_export_delete_import_releases_once() {
    native_service::native_held_checkpoint(native_service::CpuBackend::Tcg).await;
}
