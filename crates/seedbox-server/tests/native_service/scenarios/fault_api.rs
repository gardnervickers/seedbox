//! Shared public control/positive-progress checks for native fault families.
//! These are the existing CPU-case helpers, not a second execution harness.
use super::*;
use seedbox_api::{
    ControlCommand, ControlState, EventDelivery, EventKind, FaultAction, FaultStatus,
};
const TARGET: &str = "target";

pub(super) async fn step(session: &NativeSession, id: &seedbox_api::SimulationId) {
    let before = session
        .client
        .playback(id)
        .await
        .unwrap()
        .state
        .position
        .completed_rounds;
    session
        .control(id, &format!("step-{before}"), ControlCommand::Step)
        .await;
    loop {
        let state = session.client.playback(id).await.unwrap().state;
        check_bound_at(
            &session.client,
            id,
            session.backend,
            session.bound,
            state.position.simulated_time_ns,
            "the fault-case step committed",
        )
        .await;
        assert!(!state.terminal, "oracle ended during fault setup");
        if !state.stepping && state.position.completed_rounds > before {
            return;
        }
        playback_changed(session, id, state).await;
    }
}
pub(super) async fn until_output(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
    marker: &str,
) -> String {
    loop {
        let output = session.stdout(id, TARGET, false).await;
        if output.contains(marker) {
            return output;
        }
        step(session, id).await;
    }
}
pub(super) async fn inject(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
    name: &str,
    action: FaultAction,
) -> u32 {
    let receipt = session
        .control(
            id,
            name,
            ControlCommand::InjectFault {
                action: action.clone(),
            },
        )
        .await;
    let ControlState::Acknowledged {
        intervention: Some(intervention),
        error: None,
        ..
    } = receipt.state
    else {
        panic!("missing admitted fault plan")
    };
    assert_eq!(intervention.plan.action, action);
    intervention.plan.id
}
pub(super) async fn applied_once(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
    expected: &[u32],
) {
    let mut stream = session.client.events_with(id, false).await.unwrap();
    let mut applied = Vec::new();
    while let Some(delivery) = stream.next().await {
        match delivery.unwrap() {
            EventDelivery::Event { observation, .. } => match observation.event {
                EventKind::FaultChanged {
                    id,
                    status: FaultStatus::Applied,
                    ..
                } => applied.push(id),
                EventKind::OverrideRejected { .. }
                | EventKind::SourceGap { .. }
                | EventKind::SourceUnavailable => {
                    panic!("fault rejected or native observations incomplete")
                }
                _ => {}
            },
            EventDelivery::Gap { .. } => panic!("fault observation gap"),
            EventDelivery::Closed => {}
        }
    }
    assert_eq!(
        applied, expected,
        "missing, duplicated or reordered native fault applications"
    );
}
