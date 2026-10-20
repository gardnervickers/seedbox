//! Public application/machine interruption and recovery, with real guest output.
use super::*;
use seedbox_api::{ApplicationCondition, LogDelivery, LogEntry, LogStream, MachineCondition};

#[derive(Clone, Copy)]
pub enum LifecyclePair {
    StopRestart,
    ProcessPauseResume,
    MachinePauseResume,
    CrashReboot,
}
impl LifecyclePair {
    fn actions(self) -> [FaultAction; 2] {
        let machine = TARGET.to_owned();
        match self {
            Self::StopRestart => [
                FaultAction::Stop {
                    machine: machine.clone(),
                },
                FaultAction::Restart { machine },
            ],
            Self::ProcessPauseResume => [
                FaultAction::PauseProcess {
                    machine: machine.clone(),
                },
                FaultAction::ResumeProcess { machine },
            ],
            Self::MachinePauseResume => [
                FaultAction::Pause {
                    machine: machine.clone(),
                },
                FaultAction::Resume { machine },
            ],
            Self::CrashReboot => [
                FaultAction::Crash {
                    machine: machine.clone(),
                },
                FaultAction::Reboot { machine },
            ],
        }
    }
    fn pauses(self) -> bool {
        matches!(self, Self::ProcessPauseResume | Self::MachinePauseResume)
    }
    fn verify_interrupted(self, machine: &seedbox_api::MachineState) {
        match self {
            Self::StopRestart => {
                assert_eq!(machine.application.condition, ApplicationCondition::Stopped)
            }
            Self::ProcessPauseResume => {
                assert_eq!(machine.application.condition, ApplicationCondition::Paused)
            }
            Self::MachinePauseResume => assert_eq!(machine.condition, MachineCondition::Paused),
            Self::CrashReboot => assert_eq!(machine.condition, MachineCondition::Crashed),
        }
    }
}

// Preserve each real boot incarnation. The ordinary CPU helper intentionally
// rejects rebooted logs and therefore cannot be used for this crash scenario.
pub(super) async fn target_log(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
) -> (String, Vec<u32>) {
    let mut logs = session.client.logs_with(id, TARGET, false).await.unwrap();
    let mut bytes = Vec::new();
    let mut boots = Vec::new();
    while let Some(delivery) = logs.next().await {
        match delivery.unwrap() {
            LogDelivery::Entry {
                entry:
                    LogEntry::Bytes {
                        machine,
                        incarnation,
                        stream,
                        bytes: data,
                        ..
                    },
                ..
            } => {
                assert_eq!(machine, TARGET);
                assert_eq!(stream, LogStream::Stdout);
                if boots.last() != Some(&incarnation) {
                    boots.push(incarnation);
                }
                bytes.extend(data);
            }
            LogDelivery::Closed => {}
            other => panic!("incomplete lifecycle guest log: {other:?}"),
        }
    }
    (String::from_utf8(bytes).unwrap(), boots)
}

pub async fn pv_lifecycle_replay(pair: LifecyclePair) {
    native_case(move |owner| Box::pin(async move {
        let session = owner.session(CpuBackend::Pv);
        let request = cpu_request(session);
        let baseline_delta = if pair.pauses() {
            let baseline = session.submit(request.clone(), 1).await;
            session.worker(&baseline).await;
            session.complete(&baseline, false, 1, SEED).await.unwrap();
            Some(delta(&session.stdout(&baseline, TARGET, false).await))
        } else { None };
        let mut paused = request;
        paused.options.start_paused = true;
        let source = session.submit(paused, 1).await;
        session.worker(&source).await;
        session.control(&source, "lifecycle-initial-pause", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let ready = until_output(session, &source, "START=").await;
        assert!(!ready.contains("END="));
        let original_boot = session.client.world(&source).await.unwrap().machines.into_iter().find(|m| m.name == TARGET).unwrap().incarnation;
        let [interrupt, recover] = pair.actions();
        let interrupted = inject(session, &source, "lifecycle-interrupt", interrupt).await;
        step(session, &source).await;
        let world = session.client.world(&source).await.unwrap();
        pair.verify_interrupted(world.machines.iter().find(|m| m.name == TARGET).unwrap());
        if let Some(baseline) = baseline_delta {
            // Hold for twice this workload's *guest-measured* unfaulted duration.
            // The oracle keeps doing finite CPU work; no host delay or timed
            // absence establishes pause. Without actual interruption the same
            // target would finish during this committed simulated interval.
            let origin = session.client.playback(&source).await.unwrap().state.position.simulated_time_ns;
            let until = origin.checked_add(baseline.checked_mul(2).unwrap()).unwrap();
            loop {
                let now = session.client.playback(&source).await.unwrap().state.position.simulated_time_ns;
                if now >= until { break; }
                step(session, &source).await;
                let world = session.client.world(&source).await.unwrap();
                pair.verify_interrupted(world.machines.iter().find(|m| m.name == TARGET).unwrap());
            }
        }
        let (interrupted_output, _) = target_log(session, &source).await;
        assert!(!interrupted_output.contains("END="), "target ran through its interruption: {interrupted_output}");
        let recovered = inject(session, &source, "lifecycle-recover", recover).await;
        assert_ne!(interrupted, recovered);
        step(session, &source).await;
        let (output, boots) = loop {
            let observed = target_log(session, &source).await;
            if observed.0.contains("END=") { break observed; }
            step(session, &source).await;
        };
        let starts = output.matches("START=").count();
        let ends = output.matches("END=").count();
        assert_eq!(ends, 1, "recovery must finish exactly one CPU region: {output}");
        if let Some(baseline) = baseline_delta {
            assert_eq!(starts, 1, "pause/resume must preserve the original process: {output}");
            assert!(delta(&output) > baseline, "actual guest region must include its interrupted interval");
        } else {
            assert_eq!(starts, 2, "restart/reboot must actually re-enter the guest program: {output}");
        }
        let expected_boots = if matches!(pair, LifecyclePair::CrashReboot) { vec![original_boot, original_boot.checked_add(1).unwrap()] } else { vec![original_boot] };
        assert_eq!(boots, expected_boots, "application restart is not a machine reboot");
        session.control(&source, "lifecycle-finish", ControlCommand::SetRate { rate: PlaybackRate::Unlimited }).await;
        let witness = session.complete(&source, false, 1, SEED).await.unwrap();
        applied_once(session, &source, &[interrupted, recovered]).await;
        let resource = session.client.describe(&source).await.unwrap();
        assert_eq!(resource.spec.program.len(), 2);
        let replay = session.submit(SubmitRequest { metadata: Default::default(), options: Default::default(), request_id: "lifecycle-replay".into(), spec: resource.spec }, 1).await;
        session.worker(&replay).await;
        assert_eq!(session.complete(&replay, false, 1, SEED).await.unwrap(), witness);
        assert_eq!(target_log(session, &replay).await, (output.clone(), boots));
        applied_once(session, &replay, &[interrupted, recovered]).await;
        let compared = session.client.compare(&source, &replay).await.unwrap();
        assert_eq!(compared.status, ComparisonStatus::Match);
        assert_eq!(compared.witness_matches, Some(true));
        eprintln!("public lifecycle recovery MATCH source={source} replay={replay} witness={witness} starts={starts} ends={ends}");
    })).await;
}
