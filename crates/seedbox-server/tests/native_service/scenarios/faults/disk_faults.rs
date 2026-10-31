//! Named-volume durability across public machine crash/reboot, on the SDK disk.
use super::*;
use seedbox_api::MachineCondition;

fn disk_request(session: &NativeSession) -> SubmitRequest {
    let disk =
        fs::read(std::env::var_os("SEEDBOX_PV_DISK_PROBE").expect("own-source disk-probe fixture"))
            .unwrap();
    let oracle = fs::read(
        std::env::var_os("SEEDBOX_PV_CLOCK_APPLICATION").expect("own-source finite idle oracle"),
    )
    .unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    for (name, binary) in [("disk", disk), ("oracle", oracle)] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(binary.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, name, binary.as_slice())
            .unwrap();
    }
    let filesystem = session.root.path().join("disk-fault.tar");
    fs::write(&filesystem, tar.into_inner().unwrap()).unwrap();
    let mut request = fixture_submission(
        session.backend,
        session.root.path(),
        &session.store,
        SEED,
        NativeFixture {
            name: "oracle",
            filesystem,
            launch: GuestLaunch {
                argv: vec!["/oracle".into(), "idle".into()],
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
            control: false,
            request_id: "disk-fault".into(),
            extra_machines: vec![TARGET],
            volumes: vec![seedbox_spec::VolumeMount {
                source: "data".into(),
                target: "/data".into(),
                size_bytes: None,
            }],
        },
    );
    let InputSource::Resolved { definition } = &request.spec.inputs else {
        panic!("resolved disk request")
    };
    let mut definition = definition.as_ref().clone();
    definition.machines.get_mut(TARGET).unwrap().process.argv =
        vec!["/disk".into(), "volume-crash".into(), "/data".into()];
    request.spec = SimulationSpec::resolved(definition).unwrap();
    request.options.start_paused = true;
    request
}
async fn until_disk(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
    marker: &str,
) -> (String, Vec<u32>) {
    loop {
        let output = lifecycle::target_log(session, id).await;
        assert!(
            !output.0.contains("DISK error"),
            "actual guest disk operation failed: {}",
            output.0
        );
        if output.0.contains(marker) {
            return output;
        }
        step(session, id).await;
    }
}

pub async fn pv_disk_crash_reboot_replay() {
    native_case(|owner| Box::pin(async move {
        let session = owner.session(CpuBackend::Pv);
        // The real finite oracle sleeps 12.5 guest seconds. Include the normal
        // boot allowance; this is a simulated workload bound, not host speed.
        session.bound = simulated_bound(session.backend) + 12_500_000_000;
        let request = disk_request(session);
        let source = session.submit(request, 1).await;
        session.worker(&source).await;
        session.control(&source, "disk-initial-pause", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let (before, boots) = until_disk(session, &source, "DISK dirty 1\n").await;
        assert!(before.contains("DISK volume-crash boot=0 synced=- dirty=-\n"), "{before}");
        assert!(before.contains("DISK synced 1\n"), "fsync must complete before crash: {before}");
        assert_eq!(boots.len(), 1);
        let launched = session.launch_request(&source);
        assert!(matches!(launched.prepared.machines.iter().find(|m| m.name == TARGET).unwrap().disk, Some(wire::Disk::Zero { bytes }) if bytes > 0), "normal named volume must attach the real SDK disk");
        let crash = inject(session, &source, "disk-crash", FaultAction::Crash { machine: TARGET.into() }).await;
        step(session, &source).await;
        let world = session.client.world(&source).await.unwrap();
        assert_eq!(world.machines.iter().find(|m| m.name == TARGET).unwrap().condition, MachineCondition::Crashed);
        let reboot = inject(session, &source, "disk-reboot", FaultAction::Reboot { machine: TARGET.into() }).await;
        assert_ne!(crash, reboot);
        step(session, &source).await;
        let (recovered, recovered_boots) = until_disk(session, &source, "DISK dirty 2\n").await;
        // These are reads by a NEW kernel/app after volatile memory was lost:
        // the flushed count/synced bytes survive, the dirty-only file does not.
        assert!(recovered.contains("DISK volume-crash boot=1 synced=1 dirty=-\n"), "durability or volatile rollback violated: {recovered}");
        assert!(recovered.contains("DISK synced 2\n"));
        assert_eq!(recovered_boots, vec![boots[0], boots[0].checked_add(1).unwrap()]);
        session.control(&source, "disk-finish", ControlCommand::SetRate { rate: PlaybackRate::Unlimited }).await;
        let witness = session.complete(&source, false, 1, SEED).await.unwrap();
        applied_once(session, &source, &[crash, reboot]).await;
        let final_output = lifecycle::target_log(session, &source).await;
        let resource = session.client.describe(&source).await.unwrap();
        assert_eq!(resource.spec.program.len(), 2);
        let replay = session.submit(SubmitRequest { metadata: Default::default(), options: Default::default(), request_id: "disk-fault-replay".into(), spec: resource.spec }, 1).await;
        session.worker(&replay).await;
        assert_eq!(session.complete(&replay, false, 1, SEED).await.unwrap(), witness);
        assert_eq!(lifecycle::target_log(session, &replay).await, final_output);
        applied_once(session, &replay, &[crash, reboot]).await;
        let compared = session.client.compare(&source, &replay).await.unwrap();
        assert_eq!(compared.status, ComparisonStatus::Match);
        assert_eq!(compared.witness_matches, Some(true));
        eprintln!("public named-volume crash/reboot durability replay MATCH source={source} replay={replay} witness={witness}; durable=1 dirty=absent new_boot={}", recovered_boots[1]);
    })).await;
}
