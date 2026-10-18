//! Host-only service envelope/codec4 authority composition. Native socket and
//! pending guest-call continuation are separately required acceptance evidence.
use super::*;
use crate::driver_world::controller;

#[test]
fn codec5_service_envelope_preserves_controller_and_disabled_authority() {
    for enabled in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("retained");
        fs::create_dir(&output).unwrap();
        let mut inventory = fixture_inventory(&root.path().join("authority"));
        let Inventory::Pv { policy, .. } = &mut inventory else {
            unreachable!()
        };
        *policy = Some(wire::BackendPolicy {
            controller: enabled.then(|| "oracle".into()),
            stop_tolerance_ns: 91,
            ..Default::default()
        });
        let config = NetworkConfig::default();
        let latency = u64::try_from(config.latency().as_nanos()).unwrap();
        let authority = controller::resolve(&inventory, "oracle", latency).unwrap();
        let mut source = Driver::new_seeded_with_network(
            Timestamp::from_nanos(0),
            Duration::from_nanos(10),
            1,
            9338,
            || Ok(()),
            definition("oracle"),
            vec![definition("m1")],
            vec![],
            authority.configure(config.into()).unwrap(),
            |_| {
                Ok(Mock {
                    at: 0,
                    random: [0; 16],
                    first: true,
                    live_snapshots: Arc::new(AtomicUsize::new(0)),
                    peak_snapshots: Arc::new(AtomicUsize::new(0)),
                    fail_capture: false,
                })
            },
        )
        .unwrap();
        authority.validate_saved(source.controller_setup()).unwrap();
        source.step(Timestamp::from_nanos(10)).unwrap();
        let mut state = state(2);
        let manifest = capture(
            &mut source,
            Capture {
                inventory: &inventory,
                state: &mut state,
                prepared: false,
            },
            &output,
            "controller",
            publish,
            encode,
        )
        .unwrap();
        source.step(Timestamp::from_nanos(20)).unwrap();
        let expected = session_events(source.drain_events());
        drop(source);
        let envelope = read(&manifest).unwrap();
        assert_eq!(
            envelope.inventory.backend_policy(),
            inventory.backend_policy()
        );
        let checkpoint = resolve(&envelope, limits(), decode).unwrap();
        authority
            .validate_saved(checkpoint.controller_setup())
            .unwrap();
        controller::resolve(&inventory, "oracle", latency + 1)
            .unwrap()
            .validate_saved(checkpoint.controller_setup())
            .unwrap_err();
        let mut different = inventory.clone();
        let Inventory::Pv {
            policy: Some(policy),
            ..
        } = &mut different
        else {
            unreachable!()
        };
        policy.controller = (!enabled).then(|| "oracle".into());
        controller::resolve(&different, "oracle", latency)
            .unwrap()
            .validate_saved(checkpoint.controller_setup())
            .unwrap_err();
        for workers in [1, 4] {
            let mut branch = restored(&checkpoint, workers);
            authority.validate_saved(branch.controller_setup()).unwrap();
            assert!(branch.drain_events().is_empty());
            branch.step(Timestamp::from_nanos(20)).unwrap();
            assert_eq!(session_events(branch.drain_events()), expected);
        }
    }
}
