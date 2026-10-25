//! Held-before-effect capture through the public retained Driver service.

#[cfg(feature = "kvm-tests")]
#[tokio::test]
async fn pv_held_checkpoint_export_delete_import_releases_once() {
    crate::scenarios::native_held_checkpoint(crate::scenarios::CpuBackend::Pv).await;
}

#[cfg(feature = "tcg-tests")]
#[tokio::test]
async fn tcg_held_checkpoint_export_delete_import_releases_once() {
    crate::scenarios::native_held_checkpoint(crate::scenarios::CpuBackend::Tcg).await;
}
