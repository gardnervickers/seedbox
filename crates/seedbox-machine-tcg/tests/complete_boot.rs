//! Public complete-boot replacement boundary. Cold machines start no QEMU.
use seedbox_machine::{
    application::GuestEnvironment,
    machine::Machine,
    test_support::{assets, factory_definition},
};
use seedbox_machine_tcg::machine::tcg::{Factory, TcgMachine};

fn encoded(machine: &mut TcgMachine) -> Vec<u8> {
    let snapshot = machine.snapshot().unwrap();
    let mut encoded = vec![];
    snapshot.write_to(&mut encoded).unwrap();
    encoded
}
#[test]
fn supplied_complete_boot_wins_over_stale_workload_fields_without_repackaging() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("kernel"), assets::bzimage_header()).unwrap();
    std::fs::write(root.join("init"), assets::image(62)).unwrap();
    std::fs::write(root.join("command-line"), " custom=x\n").unwrap();
    std::fs::write(root.join("firmware"), [1; 64 * 1024]).unwrap();
    let mut factory = Factory::new(&std::env::current_exe().unwrap(), root, 946_684_800).unwrap();
    let original = factory_definition::definition(0);
    let mut replacement = original.clone();
    replacement.application.launch.argv = vec!["/replacement".into()];
    let mut environment = GuestEnvironment::new("node", "127.0.0.1 localhost\n", true);
    environment
        .configs
        .insert("/etc/input.bin".into(), vec![0xa5; 65_537]);
    let archive = factory
        .boot_archive_configured(&replacement, &environment)
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
    let mut configured = factory
        .create_configured(&replacement, &environment)
        .unwrap();
    let mut supplied = factory.create_boot_archive(&original, archive).unwrap();
    let mut stale = factory.create(&original).unwrap();
    let expected = encoded(&mut configured);
    assert_eq!(
        encoded(&mut supplied),
        expected,
        "complete boot is authoritative even when original application fields remain unchanged"
    );
    assert_ne!(encoded(&mut stale), expected);
}
