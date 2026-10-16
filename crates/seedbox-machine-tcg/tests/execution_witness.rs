//! Host-only cold codec checks; no guest, QEMU process or acceptance claim.
use seedbox_machine::{
    definition::BackendPolicy,
    machine::Machine,
    snapshot::DecodeLimits,
    test_support::{assets, factory_definition},
};
use seedbox_machine_tcg::machine::tcg::{Factory, Snapshot, TcgMachine};

fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 8 << 20,
        allocation_bytes: 32 << 20,
        collection_items: 65536,
        total_items: 262144,
        string_bytes: 1 << 20,
        blob_bytes: 8 << 20,
    }
}
fn wire(s: &Snapshot) -> Vec<u8> {
    let mut bytes = vec![];
    s.write_to(&mut bytes).unwrap();
    bytes
}
#[test]
fn cold_native_absence_and_codec_restore_do_not_manufacture_execution() {
    for full_state in [false, true] {
        let root = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("kernel", assets::bzimage_header()),
            ("init", assets::image(62)),
            ("command-line", b"console=ttyS0".to_vec()),
            ("firmware", vec![1; 64 * 1024]),
        ] {
            std::fs::write(root.path().join(name), bytes).unwrap();
        }
        let executable = root.path().join("never-executed");
        std::fs::write(&executable, b"not executable").unwrap();
        let mut factory = Factory::new(&executable, root.path(), 946_684_800)
            .unwrap()
            .with_policy(BackendPolicy {
                full_state,
                ..Default::default()
            })
            .unwrap();
        let mut machine = factory.create(&factory_definition::definition(0)).unwrap();
        let before = wire(&machine.snapshot().unwrap());
        assert_eq!(&before[8..12], &3u32.to_le_bytes());
        for _ in 0..3 {
            assert!(machine.execution_evidence().unwrap().is_none());
            assert!(machine.state_evidence().unwrap().is_none());
            assert_eq!(wire(&machine.snapshot().unwrap()), before);
        }
        drop(machine);
        drop(factory);
        std::fs::remove_dir_all(root.path()).unwrap();
        for _ in 0..2 {
            let s = Snapshot::read_from(&mut &before[..], &limits()).unwrap();
            assert_eq!(wire(&s), before);
            let mut restored = TcgMachine::from_snapshot(&executable, &s).unwrap();
            assert!(restored.execution_evidence().unwrap().is_none());
            assert_eq!(wire(&restored.snapshot().unwrap()), before);
        }
    }
}
