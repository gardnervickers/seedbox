use super::*;

#[test]
fn pure_configured_assembly_is_exact_creation_input_including_the_pv_budget() {
    let mut encodings = vec![];
    for budget in [1, 127] {
        let directory = tempfile::tempdir().unwrap();
        assets(directory.path());
        let mut factory = Factory::new(
            &std::env::current_exe().unwrap(),
            directory.path(),
            crate::machine::TEST_EPOCH_SECONDS,
        )
        .unwrap()
        .with_progress_budget(std::num::NonZeroU32::new(budget).unwrap());
        let mut definition = definition::definition(0);
        let original = definition.clone();
        let mut environment =
            crate::application::GuestEnvironment::new("node", "127.0.0.1 localhost\n", true);
        environment
            .configs
            .insert("/etc/large.bin".into(), vec![0xa5; 65_537]);
        fs::remove_dir_all(directory.path()).unwrap();
        definition.resources.memory_bytes = 1;
        let expected = factory
            .boot_archive_configured(&definition, &environment)
            .unwrap();
        assert!(expected.len() > 65_537);
        assert!(factory
            .create_configured(&definition, &environment)
            .is_err());
        definition.resources.memory_bytes = original.resources.memory_bytes;
        assert_eq!(
            factory
                .boot_archive_configured(&definition, &environment)
                .unwrap(),
            expected
        );
        let machine = factory
            .create_configured(&definition, &environment)
            .unwrap();
        assert_eq!(machine.initrd, expected);
        let direct = factory
            .create_boot_archive(&definition, expected.clone())
            .unwrap();
        assert_eq!(
            direct.initrd, expected,
            "complete boot bytes must be used verbatim"
        );
        assert_eq!(definition, original);
        assert!(expected
            .windows(65_537)
            .any(|part| part.iter().all(|byte| *byte == 0xa5)));
        encodings.push(expected);
    }
    assert_ne!(
        encodings[0], encodings[1],
        "selected instrumentation budget must change actual boot bytes"
    );
}
