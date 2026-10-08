use seedbox_engine_core::disk;
// Exercise the actual retained-disk model on portable hosts, without KVM.

#[test]
fn disk_fault_model_is_parameterized_without_a_wire_name() {
    let model = disk::parse_disk_fault_model(&serde_json::json!({
        "sector_bytes": 512,
        "survival_probability": 0.5,
    }))
    .unwrap();
    assert_eq!(
        model,
        disk::DiskFaultModel {
            sector_bytes: disk::SECTOR,
            survival_probability: 0.5
        }
    );
}

#[test]
fn committed_torn_disk_world_uses_disk_loss_object() {
    let world: serde_json::Value =
        serde_json::from_str(include_str!("../../../harness/worlds/disk-ext4-torn.json")).unwrap();
    let model = disk::parse_disk_fault_model(&world["disk_loss"]).unwrap();
    assert_eq!(model.sector_bytes, disk::SECTOR);
    assert_eq!(model.survival_probability, 0.5);
    assert!(world["disk_loss"].is_object());
}

#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-engine-core/src/checkpoint.rs"]
mod checkpoint;
