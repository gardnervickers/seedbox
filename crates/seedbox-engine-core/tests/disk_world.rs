use seedbox_engine_core::disk;
#[test]
fn committed_torn_disk_world_uses_disk_loss_object() {
    let world: seedbox_engine_core::world_config::WorldConfig =
        serde_json::from_str(include_str!("../../../harness/worlds/disk-ext4-torn.json")).unwrap();
    let model = disk::fault_model(world.disk_loss).unwrap();
    assert_eq!(model.sector_bytes, disk::SECTOR);
    assert_eq!(model.survival_probability, 0.5);
}
