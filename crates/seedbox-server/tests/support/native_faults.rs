//! Real public fault injection and retained replay. No fixture engine results.
use super::*;
use seedbox_api::{ControlCommand, FaultAction, InputSource, PlaybackRate};
#[cfg(feature = "worker-failure-tests")]
#[path = "native_fault_worker_failure.rs"]
mod worker_failure;
#[cfg(feature = "worker-failure-tests")]
pub use worker_failure::pv_fault_worker_failure;

#[path = "native_fault_cancel.rs"]
mod cancellation;
pub use cancellation::pv_fault_cancel;

#[path = "native_fault_lifecycle.rs"]
mod lifecycle;
pub use lifecycle::{pv_lifecycle_replay, LifecyclePair};

const SEED: u64 = 123;
const TARGET: &str = "target";
const HANDLE: &str = "public-throttle";

fn cpu_request(session: &NativeSession) -> SubmitRequest {
    // Five times as much oracle work keeps it alive while the target performs
    // its finite region at twice the CPU cost. This is guest work, not a host
    // delay; the shared simulated bound applies to every step.
    let mut request = submission_with_machines(
        DriverBackend::Pv,
        session.root.path(),
        &session.store,
        2_000_000,
        SEED,
        vec![TARGET],
    );
    let InputSource::Resolved { definition } = &request.spec.inputs else {
        panic!("resolved CPU fixture")
    };
    let mut definition = definition.as_ref().clone();
    definition
        .machines
        .get_mut(TARGET)
        .expect("CPU target has its own resolved map entry")
        .process
        .argv = vec!["/application".into(), "400000".into()];
    request.spec = SimulationSpec::resolved(definition).unwrap();
    request
}
fn delta(output: &str) -> u64 {
    output
        .split_whitespace()
        .find_map(|word| word.strip_prefix("DELTA="))
        .expect("target completed its finite CPU region")
        .parse()
        .unwrap()
}
/// Establish a real, still-live guest under one applied public fault. The
/// caller selects cancellation or original-worker failure only AFTER this
/// positive application/fault/paused boundary, never by a host-time delay.
async fn active_throttle(session: &mut NativeSession) -> (seedbox_api::SimulationId, u32) {
    let mut request = cpu_request(session);
    request.options.start_paused = true;
    let id = session.submit(request, 1).await;
    session.worker(&id).await;
    session
        .control(
            &id,
            "active-initial-pause",
            ControlCommand::SetRate {
                rate: PlaybackRate::Paused,
            },
        )
        .await;
    let output = until_output(session, &id, "START=").await;
    assert!(
        !output.contains("END="),
        "target ended before the active-fault scenario"
    );
    let fault = inject(
        session,
        &id,
        "active-throttle",
        FaultAction::Throttle {
            handle: HANDLE.into(),
            machine: TARGET.into(),
            factor_permille: 2_000,
        },
    )
    .await;
    step(session, &id).await;
    let world = session.client.world(&id).await.unwrap();
    assert_eq!(
        world
            .machines
            .iter()
            .find(|machine| machine.name == TARGET)
            .unwrap()
            .throttle_permille,
        Some(2_000)
    );
    applied_once(session, &id, &[fault]).await;
    let paused = session.client.playback(&id).await.unwrap().state;
    assert_eq!(paused.rate, PlaybackRate::Paused);
    assert!(!paused.terminal && !paused.stepping);
    assert!(!session.stdout(&id, TARGET, false).await.contains("END="));
    (id, fault)
}

pub async fn pv_throttle_heal_replay() {
    native_case(|owner| Box::pin(async move {
        let session = owner.session(DriverBackend::Pv);
        let request = cpu_request(session);
        let baseline = session.submit(request.clone(), 1).await;
        session.worker(&baseline).await;
        session.complete(&baseline, false, 1, SEED).await.unwrap();
        let baseline_output = session.stdout(&baseline, TARGET, false).await;
        let mut paused = request;
        paused.options.start_paused = true;
        let source = session.submit(paused, 1).await;
        session.worker(&source).await;
        session.control(&source, "initial-pause", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let started = until_output(session, &source, "START=").await;
        assert!(!started.contains("END="), "target region ended before throttle admission");
        let fault = inject(session, &source, "throttle", FaultAction::Throttle { handle: HANDLE.into(), machine: TARGET.into(), factor_permille: 2_000 }).await;
        step(session, &source).await;
        let world = session.client.world(&source).await.unwrap();
        assert_eq!(world.machines.iter().find(|machine| machine.name == TARGET).unwrap().throttle_permille, Some(2_000));
        let target_output = until_output(session, &source, "END=").await;
        assert!(delta(&target_output) > delta(&baseline_output), "throttle must change actual guest-observed CPU-region time");
        let heal = inject(session, &source, "heal", FaultAction::Heal { handle: HANDLE.into() }).await;
        assert_ne!(fault, heal);
        step(session, &source).await;
        let world = session.client.world(&source).await.unwrap();
        assert_ne!(world.machines.iter().find(|machine| machine.name == TARGET).unwrap().throttle_permille, Some(2_000));
        session.control(&source, "finish", ControlCommand::SetRate { rate: PlaybackRate::Unlimited }).await;
        let source_witness = session.complete(&source, false, 1, SEED).await.unwrap();
        applied_once(session, &source, &[fault, heal]).await;
        // Consume the service-frozen spec, not fresh duplicate host injections.
        let resource = session.client.describe(&source).await.unwrap();
        assert_eq!(resource.spec.program.len(), 2);
        let replay = session.submit(SubmitRequest { metadata: Default::default(), options: Default::default(), request_id: "fault-replay".into(), spec: resource.spec }, 1).await;
        session.worker(&replay).await;
        let replay_witness = session.complete(&replay, false, 1, SEED).await.unwrap();
        assert_eq!(source_witness, replay_witness);
        assert_eq!(target_output, session.stdout(&replay, TARGET, false).await);
        applied_once(session, &replay, &[fault, heal]).await;
        let comparison = session.client.compare(&source, &replay).await.unwrap();
        assert_eq!(comparison.status, ComparisonStatus::Match);
        assert_eq!(comparison.witness_matches, Some(true));
        eprintln!("public throttle/heal native replay MATCH source={source} replay={replay} witness={source_witness} guest_delta={} baseline_delta={}", delta(&target_output), delta(&baseline_output));
    })).await;
}
