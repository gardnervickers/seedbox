//! Core composition of the real public cold PV factory/codec3. No worker,
//! hardware, guest execution or native-evidence authenticity is claimed.
use super::*;
use seedbox_machine::{
    definition::BackendPolicy,
    test_support::{assets, factory_definition},
};

fn cold(policy: Option<BackendPolicy>) -> (String, pv::Snapshot) {
    let root = tempfile::tempdir().unwrap();
    #[cfg(target_arch = "x86_64")]
    let (machine, kernel, loader) = {
        let mut kernel = assets::bzimage_header();
        kernel[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
        (62, kernel, "lib/ld-linux-x86-64.so.2")
    };
    #[cfg(target_arch = "aarch64")]
    let (machine, kernel, loader) = {
        let mut kernel = vec![0; 4096];
        kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
        kernel[56..60].copy_from_slice(b"ARM\x64");
        (183, kernel, "lib/ld-linux-aarch64.so.1")
    };
    std::fs::write(root.path().join("kernel"), kernel).unwrap();
    std::fs::write(root.path().join("init"), assets::image(machine)).unwrap();
    std::fs::write(root.path().join("command-line"), b"rdinit=/init").unwrap();
    // These data-only asset names are the public offline PV runtime contract.
    // The test does not duplicate backend protocol or physical state machinery.
    let mut tar = tar::Builder::new(vec![]);
    for name in [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        loader,
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ] {
        let bytes = if name.ends_with(".txt") || name.ends_with(".LIB") {
            b"fixture license".to_vec()
        } else {
            assets::image(machine)
        };
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_size(bytes.len() as u64);
        tar.append_data(&mut header, format!("seedbox/progress/{name}"), &bytes[..])
            .unwrap();
    }
    std::fs::write(root.path().join("progress.tar"), tar.into_inner().unwrap()).unwrap();
    let factory = pv::Factory::new(&std::env::current_exe().unwrap(), root.path(), 86400).unwrap();
    let mut factory = match policy {
        Some(policy) => factory.with_policy(policy).unwrap(),
        None => factory,
    };
    let definition = factory_definition::definition(0);
    let mut machine = factory
        .create_boot_archive(&definition, b"owned cold complete boot bytes".to_vec())
        .unwrap();
    (definition.name, machine.snapshot().unwrap())
}
fn inventory(policy: Option<wire::BackendPolicy>) -> Inventory {
    let artifact = wire::Artifact {
        sha256: "00".repeat(32),
        length: 0,
        artifact: "/absent-pv-native-input".into(),
    };
    Inventory::Pv {
        policy,
        executable: artifact.clone(),
        kernel: artifact.clone(),
        init: artifact.clone(),
        command_line: artifact.clone(),
        progress: artifact.clone(),
        runtime: artifact,
        formatter: None,
        environments: Default::default(),
        boot_archives: Default::default(),
        blocks_per_progress_call: std::num::NonZeroU32::new(32).unwrap(),
        epoch_seconds: 86400,
    }
}
#[test]
fn codec3_policy_binding_refuses_before_native_restore_and_only_x86_linux_restores_cold_pv() {
    let policy = BackendPolicy::default();
    let (name, snapshot) = cold(Some(policy.clone()));
    let mut encoded = vec![];
    snapshot.write_to(&mut encoded).unwrap();
    let inventory = inventory(Some(crate::driver_world::policy::encode(&policy).unwrap()));
    let limits = seedbox_machine::snapshot::DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 64 << 20,
        collection_items: 1 << 20,
        total_items: 1 << 20,
        string_bytes: 1 << 20,
        blob_bytes: 16 << 20,
    };
    let snapshot = Snapshot::read_from(&inventory, &mut &encoded[..], &limits).unwrap();
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("no-native-files");
    let restore = |name: String, inventory: &Inventory| {
        Native::restore(name, inventory, &missing, &snapshot, &limits, &missing)
            .err()
            .expect("invalid authority must refuse")
    };
    let error = restore("wrong-name".into(), &inventory);
    assert!(error.contains("policy"), "{error}");
    let mut none = inventory.clone();
    let Inventory::Pv { policy, .. } = &mut none else {
        unreachable!()
    };
    *policy = None;
    let error = restore(name.clone(), &none);
    assert!(error.contains("policy"), "{error}");
    let restored = Native::restore(
        name,
        &inventory,
        &std::env::current_exe().unwrap(),
        &snapshot,
        &limits,
        root.path(),
    );
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    assert!(restored.is_ok(), "valid cold restore must remain host-only");
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    assert!(restored.err().unwrap().contains("incomplete"));
    assert!(!missing.exists());
}
#[test]
fn codec3_ordinary_none_is_not_explicit_default_and_closed_body_is_checked() {
    let (name, snapshot) = cold(None);
    snapshot.validate_policy(&name, None).unwrap();
    assert!(snapshot
        .validate_policy(&name, Some(&BackendPolicy::default()))
        .is_err());
    let mut bytes = vec![];
    snapshot.write_to(&mut bytes).unwrap();
    let mut truncated = bytes.clone();
    truncated.pop();
    let limits = seedbox_machine::snapshot::DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 64 << 20,
        collection_items: 1 << 20,
        total_items: 1 << 20,
        string_bytes: 1 << 20,
        blob_bytes: 16 << 20,
    };
    assert!(Snapshot::read_from(&inventory(None), &mut &truncated[..], &limits).is_err());
    let decoded = Snapshot::read_from(&inventory(None), &mut &bytes[..], &limits).unwrap();
    let Snapshot::Pv(decoded) = decoded else {
        panic!("PV body must stay PV")
    };
    decoded.validate_policy(&name, None).unwrap();
}

#[test]
fn pv_controller_restore_remains_scoped_to_linux_x86() {
    let policy = wire::BackendPolicy {
        controller: Some("oracle".into()),
        ..Default::default()
    };
    let admitted = crate::driver_world::policy::admit_restore(&inventory(Some(policy)));
    assert_eq!(
        admitted.is_ok(),
        cfg!(all(target_os = "linux", target_arch = "x86_64"))
    );
}
