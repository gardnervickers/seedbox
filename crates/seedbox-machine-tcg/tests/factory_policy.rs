//! Cold factory policy funnels. No QEMU process, guest execution or native proof.
use seedbox_machine::{
    application::GuestEnvironment,
    definition::BackendPolicy,
    machine::Machine,
    test_support::{assets, factory_definition},
};
use seedbox_machine_tcg::machine::tcg::Factory;
fn assets_directory() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("kernel", assets::bzimage_header()),
        ("init", assets::image(62)),
        ("command-line", b"console=ttyS0".to_vec()),
        ("firmware", vec![1; 64 * 1024]),
    ] {
        std::fs::write(root.path().join(name), bytes).unwrap();
    }
    root
}
#[test]
fn every_fresh_funnel_closes_exact_selected_policy_before_any_native_start() {
    for funnel in 0..3 {
        let root = assets_directory();
        let definition = factory_definition::definition(0);
        let policy = BackendPolicy {
            seed: 73,
            controller: Some(definition.name.clone()),
            full_state: true,
            boot_end_evidence: true,
            ..Default::default()
        };
        let mut factory = Factory::new(&std::env::current_exe().unwrap(), root.path(), 946_684_800)
            .unwrap()
            .with_policy(policy.clone())
            .unwrap();
        let environment = GuestEnvironment::new("node", "127.0.0.1 localhost\n", true);
        let mut machine = match funnel {
            0 => factory.create(&definition).unwrap(),
            1 => factory
                .create_configured(&definition, &environment)
                .unwrap(),
            _ => {
                let archive = factory
                    .boot_archive_configured(&definition, &environment)
                    .unwrap();
                factory.create_boot_archive(&definition, archive).unwrap()
            }
        };
        let snapshot = machine.snapshot().unwrap();
        snapshot
            .validate_policy(&definition.name, Some(&policy))
            .unwrap();
        assert!(snapshot.validate_policy(&definition.name, None).is_err());
        assert!(snapshot
            .validate_policy("other-machine", Some(&policy))
            .is_err());
        // Never admitted a physical boot: C2 requires absence, not an error or
        // fabricated RAM. This remains a cold contract, not native evidence.
        assert!(machine.state_evidence().unwrap().is_none());
    }
}
#[test]
fn ordinary_none_and_explicit_default_are_not_interchangeable() {
    let root = assets_directory();
    let definition = factory_definition::definition(0);
    let mut factory =
        Factory::new(&std::env::current_exe().unwrap(), root.path(), 946_684_800).unwrap();
    let mut ordinary = factory.create(&definition).unwrap();
    let snapshot = ordinary.snapshot().unwrap();
    snapshot.validate_policy(&definition.name, None).unwrap();
    assert!(snapshot
        .validate_policy(&definition.name, Some(&BackendPolicy::default()))
        .is_err());
    let mut selected = factory
        .with_policy(BackendPolicy::default())
        .unwrap()
        .create(&definition)
        .unwrap();
    let snapshot = selected.snapshot().unwrap();
    snapshot
        .validate_policy(&definition.name, Some(&BackendPolicy::default()))
        .unwrap();
    assert!(snapshot.validate_policy(&definition.name, None).is_err());
    let bad = BackendPolicy {
        controller: Some(String::new()),
        ..Default::default()
    };
    assert!(
        Factory::new(&std::env::current_exe().unwrap(), root.path(), 946_684_800)
            .unwrap()
            .with_policy(bad)
            .is_err()
    );
}
