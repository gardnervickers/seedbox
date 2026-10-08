// Exercise the actual retained-disk model on portable hosts, without KVM.
#[allow(dead_code, clippy::manual_is_multiple_of)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/disk.rs"]
mod disk;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/rng.rs"]
mod rng;

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
