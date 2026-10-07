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

#[test]
fn named_rng_streams_preserve_draw_order_without_cross_domain_consumption() {
    let mut timer = rng::Rng::new(1009, "timer-jitter/workload");
    let mut link = rng::Rng::new(1009, "link/a/b");
    // Golden draws pin the existing stream version, domain and draw positions.
    assert_eq!(timer.next(), 701_739_635_931_870_289);
    assert_eq!(link.next(), 7_715_882_015_694_847_740);
    assert_eq!(timer.below(0), 0); // Disabled sampling consumes no draw.
    assert_eq!(timer.below(1), 0); // Enabled sampling consumes exactly one here.
    assert_eq!(timer.next(), 16_142_657_131_779_246_564);
    assert_eq!(link.next(), 4_670_307_978_545_747_654);
    assert_eq!(timer.next(), 1_281_788_170_858_284_925);
    assert_eq!(link.next(), 9_164_057_344_090_358_647);
}
