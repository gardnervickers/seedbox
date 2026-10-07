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
fn profile_and_engine_name_the_same_disk_semantics() {
    assert_eq!(seedbox::vmm::TORN_DISK_MODEL, disk::TORN_MODEL);
}
