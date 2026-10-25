//! Network fault families on the shared network-probe world, through the real
//! retained Driver service. The faults are public program inputs placed in the
//! client's TCP transfer: a partition or a directed shape, each later healed.
//! The guest's own TCP completion shows the effect; replay of the frozen spec
//! must reach the same native witness and output.
use super::*;
use seedbox_api::{EventDelivery, EventKind, FaultStatus};

const HANDLE: &str = "network-fault";
const SEED: u64 = 123;

/// When an unfaulted run starts and ends its TCP transfer, in simulated ns.
struct Transfer {
    begin_ns: u64,
    end_ns: u64,
    final_ns: u64,
    witness: String,
    client: String,
}

/// `request` carrying `inputs` (key, at_ns, public action) as its program.
fn with_inputs(
    mut request: SubmitRequest,
    inputs: &[(&str, u64, serde_json::Value)],
) -> SubmitRequest {
    let seedbox_api::InputSource::Resolved { definition } = &request.spec.inputs else {
        panic!("resolved submission required")
    };
    let mut definition = definition.as_ref().clone();
    for (key, at_ns, action) in inputs {
        definition
            .overrides
            .push(seedbox_spec::definition::Override {
                id: (*key).into(),
                at_ns: *at_ns,
                action: action.clone(),
                timeout_ns: seedbox_spec::definition::default_timeout_ns(),
            });
    }
    request.spec = SimulationSpec::resolved(definition).unwrap();
    assert_eq!(request.spec.program.len(), inputs.len());
    request
}

/// Applied fault ids, in order, and the plans' public action names.
async fn applied(session: &NativeSession, id: &seedbox_api::SimulationId) -> Vec<(u32, String)> {
    let mut stream = session.client.events_with(id, false).await.unwrap();
    let mut applied = Vec::new();
    while let Some(delivery) = stream.next().await {
        match delivery.unwrap() {
            EventDelivery::Event { observation, .. } => match observation.event {
                EventKind::FaultChanged {
                    id,
                    action,
                    status: FaultStatus::Applied,
                    ..
                } => applied.push((id, action)),
                EventKind::OverrideRejected {
                    input_key, reason, ..
                } => {
                    panic!("network fault input {input_key} rejected: {reason}")
                }
                EventKind::SourceGap { .. } | EventKind::SourceUnavailable => {
                    panic!("native fault observations incomplete")
                }
                _ => {}
            },
            EventDelivery::Gap { .. } => panic!("fault observation gap"),
            EventDelivery::Closed => {}
        }
    }
    applied
}

async fn unfaulted(session: &mut NativeSession, backend: CpuBackend) -> Transfer {
    let request = network_submission(backend, session.root.path(), &session.store, SEED);
    let id = session.submit(request, 1).await;
    session.worker(&id).await;
    let witness = session.complete(&id, false, 1, SEED).await.unwrap();
    let timed = session.stdout_timed(&id, "client", false).await;
    let transfer = Transfer {
        begin_ns: marker_time(&timed, PROBE_CLIENT_MID),
        end_ns: marker_time(&timed, PROBE_TCP_DONE),
        final_ns: session
            .client
            .describe(&id)
            .await
            .unwrap()
            .status
            .simulated_time_ns,
        witness,
        client: timed.0,
    };
    assert!(transfer.begin_ns < transfer.end_ns && transfer.end_ns < IDLE_SLEEP_NS);
    assert!(transfer.client.ends_with("NETPROBE done\n"));
    transfer
}

/// Run `inputs` on the network world, then replay its frozen spec; return the
/// faulted client's TCP completion time and the world after the source run.
async fn faulted_and_replayed(
    session: &mut NativeSession,
    backend: CpuBackend,
    inputs: &[(&str, u64, serde_json::Value)],
    actions: &[&str],
) -> (u64, seedbox_api::WorldState, String) {
    let request = with_inputs(
        network_submission(backend, session.root.path(), &session.store, SEED),
        inputs,
    );
    let source = session.submit(request, 1).await;
    session.worker(&source).await;
    let source_witness = session.complete(&source, false, 1, SEED).await.unwrap();
    let world = session.client.world(&source).await.unwrap();
    let timed = session.stdout_timed(&source, "client", false).await;
    assert_eq!(
        count(&timed.0, PROBE_TCP_DONE),
        1,
        "client did not recover: {}",
        timed.0
    );
    assert!(timed.0.ends_with("NETPROBE done\n"), "{}", timed.0);
    assert_eq!(session.stdout(&source, "server", false).await, PROBE_READY);
    let source_applied = applied(session, &source).await;
    assert_eq!(
        source_applied
            .iter()
            .map(|(_, action)| action.as_str())
            .collect::<Vec<_>>(),
        actions,
        "each input applied exactly once, in order"
    );
    // Replay consumes the service-frozen spec; it injects nothing itself.
    let resource = session.client.describe(&source).await.unwrap();
    assert_eq!(resource.spec.program.len(), inputs.len());
    let replay = session
        .submit(
            SubmitRequest {
                fault_profile: None,
                metadata: Default::default(),
                options: Default::default(),
                request_id: "network-fault-replay".into(),
                spec: resource.spec,
            },
            1,
        )
        .await;
    session.worker(&replay).await;
    let replay_witness = session.complete(&replay, false, 1, SEED).await.unwrap();
    assert_eq!(
        source_witness, replay_witness,
        "raw native witnesses differ"
    );
    for machine in ["oracle", "server", "client"] {
        assert_eq!(
            session.stdout(&source, machine, false).await,
            session.stdout(&replay, machine, false).await,
            "{machine} output differs on replay"
        );
    }
    assert_eq!(applied(session, &replay).await, source_applied);
    let comparison = session.client.compare(&source, &replay).await.unwrap();
    assert_eq!(comparison.status, ComparisonStatus::Match);
    assert_eq!(comparison.witness_matches, Some(true));
    eprintln!(
        "network fault replay MATCH backend={} source={source} replay={replay} witness={source_witness} applied={source_applied:?}",
        label(backend)
    );
    (marker_time(&timed, PROBE_TCP_DONE), world, source_witness)
}

fn link<'a>(
    world: &'a seedbox_api::WorldState,
    from: &str,
    to: &str,
) -> &'a seedbox_api::LinkState {
    world
        .links
        .iter()
        .find(|link| link.from == from && link.to == to)
        .unwrap_or_else(|| panic!("{from}->{to} link: {:?}", world.links))
}

/// Cut client|server both ways mid-transfer for one simulated second, then
/// heal: the stream stalls, frames are dropped by the partition, and TCP
/// recovers to the same complete echo after the heal.
pub async fn network_partition_heal_replay(backend: CpuBackend) {
    native_case(move |owner| {
        Box::pin(async move {
            let session = owner.session(backend);
            session.bound = network_bound(backend);
            let base = unfaulted(session, backend).await;
            let cut_ns = base.begin_ns + (base.end_ns - base.begin_ns) / 2;
            let heal_ns = cut_ns + 1_000_000_000;
            let inputs = [
                (
                    "partition",
                    cut_ns,
                    serde_json::json!({"action": "partition", "handle": HANDLE, "left": ["client"], "right": ["server"]}),
                ),
                ("heal", heal_ns, serde_json::json!({"action": "heal", "handle": HANDLE})),
            ];
            let (done_ns, world, witness) =
                faulted_and_replayed(session, backend, &inputs, &["partition", "heal"]).await;
            assert_ne!(witness, base.witness, "the partition must change the run");
            // The guest could not finish its echo while cut off.
            assert!(done_ns > heal_ns, "TCP completed at {done_ns} before the heal at {heal_ns}");
            let dropped: u64 = [("client", "server"), ("server", "client")]
                .iter()
                .map(|(from, to)| link(&world, from, to).counters.partition_dropped)
                .sum();
            assert!(dropped > 0, "no frame was dropped by the partition: {:?}", world.links);
            assert!(world.links.iter().all(|l| l.partitioned_by.is_none()), "partition still active after heal");
            eprintln!(
                "partition/heal: backend={} cut_ns={cut_ns} heal_ns={heal_ns} unfaulted_done_ns={} faulted_done_ns={done_ns} partition_dropped={dropped} unfaulted_final_ns={}",
                label(backend),
                base.end_ns,
                base.final_ns
            );
        })
    })
    .await;
}

/// Shape client->server mid-transfer (added latency, loss, duplicates and
/// reordering), then heal: the transfer slows and loses frames, and TCP still
/// delivers the exact echo.
pub async fn network_shape_heal_replay(backend: CpuBackend) {
    native_case(move |owner| {
        Box::pin(async move {
            let session = owner.session(backend);
            session.bound = network_bound(backend);
            let base = unfaulted(session, backend).await;
            let shape_ns = base.begin_ns + (base.end_ns - base.begin_ns) / 2;
            let heal_ns = shape_ns + 500_000_000;
            let inputs = [
                (
                    "shape",
                    shape_ns,
                    serde_json::json!({
                        "action": "shape", "handle": HANDLE, "from": ["client"], "to": ["server"],
                        "latency_ns": 20_000_000u64, "jitter_ns": 0u64, "loss_ppm": 100_000u64,
                        "network_faults": {
                            "duplicate_ppm": 50_000u64,
                            "reorder_ppm": 50_000u64, "reorder_delay_ns": 2_000_000u64
                        }
                    }),
                ),
                ("heal", heal_ns, serde_json::json!({"action": "heal", "handle": HANDLE})),
            ];
            let (done_ns, world, witness) =
                faulted_and_replayed(session, backend, &inputs, &["shape", "heal"]).await;
            assert_ne!(witness, base.witness, "the shape must change the run");
            assert!(done_ns > base.end_ns, "shaped TCP finished at {done_ns}, unfaulted at {}", base.end_ns);
            let up = link(&world, "client", "server");
            assert!(up.counters.loss_dropped > 0, "no frame lost under the shape: {up:?}");
            assert!(up.shaped_by.is_none(), "shape still active after heal: {up:?}");
            eprintln!(
                "shape/heal: backend={} shape_ns={shape_ns} heal_ns={heal_ns} unfaulted_done_ns={} faulted_done_ns={done_ns} loss_dropped={} scheduled={}",
                label(backend),
                base.end_ns,
                up.counters.loss_dropped,
                up.counters.scheduled
            );
        })
    })
    .await;
}

/// The drop world: the oracle is network-probe `drop-watch`, holding the
/// guest control socket with no network interface (a controller is outside
/// the failure model); `drop-client` talks to the ordinary echo server.
const DROP_MACHINES: [ProbeMachine; 3] = [
    ProbeMachine {
        name: "oracle",
        binary: "SEEDBOX_PV_NETWORK_APPLICATION",
        argv: &["drop-watch"],
        interface: None,
    },
    ProbeMachine {
        name: "server",
        binary: "SEEDBOX_PV_NETWORK_APPLICATION",
        argv: &["server", PROBE_PORT],
        interface: Some((1, [10, 0, 0, 1])),
    },
    ProbeMachine {
        name: "client",
        binary: "SEEDBOX_PV_NETWORK_APPLICATION",
        argv: &["drop-client", "10.0.0.1", PROBE_PORT],
        interface: Some((2, [10, 0, 0, 2])),
    },
];
const DROP_CONNECTION: &str = "NETPROBE drop connection=";
const DROP_ABORTED: &str = "NETPROBE drop aborted errno=110 ";
const DROP_REINCARNATION: &str = "NETPROBE drop reincarnation old=";

/// Commit one more round of a paused simulation through the public Step.
async fn step(session: &NativeSession, id: &seedbox_api::SimulationId) {
    use seedbox_api::ControlCommand;
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
        // An oracle that exits ends the worker; the caller then reports why.
        let Ok(playback) = session.client.playback(id).await else {
            return;
        };
        let state = playback.state;
        check_bound_at(
            &session.client,
            id,
            session.backend,
            session.bound,
            state.position.simulated_time_ns,
            "the connection-drop step committed",
        )
        .await;
        if state.terminal {
            return;
        }
        if !state.stepping && state.position.completed_rounds > before {
            return;
        }
        tokio::task::yield_now().await;
    }
}
/// Step until `machine`'s committed stdout contains `marker`.
async fn until_output(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
    machine: &str,
    marker: &str,
) -> String {
    loop {
        let output = session.stdout(id, machine, false).await;
        if output.contains(marker) {
            return output;
        }
        let phase = session.client.describe(id).await.unwrap().status.phase;
        assert_ne!(
            phase,
            seedbox_api::Phase::Completed,
            "run ended before {machine} printed {marker:?}: {output} oracle: {}",
            session.stdout(id, "oracle", false).await
        );
        step(session, id).await;
    }
}
/// Inject `action` live through the public control API; its admitted plan id.
async fn inject(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
    name: &str,
    action: seedbox_api::FaultAction,
) -> u32 {
    use seedbox_api::{ControlCommand, ControlState};
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
        panic!("{name}: no admitted fault plan: {receipt:?}")
    };
    assert_eq!(intervention.plan.action, action);
    intervention.plan.id
}

/// Drop the server-to-client direction of an observed TCP incarnation. The
/// oracle reads the connection through the guest control API and prints it;
/// the host parses that public log line and injects the drop live, then the
/// heal once the client reports its connection aborted. The client then
/// reconnects, and the oracle observes a newer incarnation of the same
/// direction.
pub async fn network_drop_connection_replay(backend: CpuBackend) {
    use seedbox_api::{ControlCommand, FaultAction, PlaybackRate};
    native_case(move |owner| {
        Box::pin(async move {
            let session = owner.session(backend);
            session.bound = network_bound(backend);
            let mut request = network_world_submission(
                backend,
                session.root.path(),
                &session.store,
                SEED,
                &DROP_MACHINES,
                true,
            );
            request.options.start_paused = true;
            let source = session.submit(request, 1).await;
            session.worker(&source).await;
            session
                .control(&source, "drop-initial-pause", ControlCommand::SetRate { rate: PlaybackRate::Paused })
                .await;
            let observed = until_output(session, &source, "oracle", DROP_CONNECTION).await;
            let line = observed
                .lines()
                .find_map(|line| line.strip_prefix(DROP_CONNECTION))
                .expect("observed connection line");
            let connection: seedbox_engine_protocol::control::Connection =
                serde_json::from_str(line).expect("oracle printed the control API's connection object");
            assert_eq!(
                (connection.from.as_str(), connection.to.as_str(), connection.src_port),
                ("server", "client", PROBE_PORT.parse().unwrap())
            );
            assert!(
                session.stdout(&source, "client", false).await.contains("NETPROBE drop first ok"),
                "the observed incarnation has carried an echo"
            );
            let drop = inject(
                session,
                &source,
                "drop-connection",
                FaultAction::DropConnection { handle: HANDLE.into(), connection: connection.clone() },
            )
            .await;
            let aborted = until_output(session, &source, "client", DROP_ABORTED).await;
            assert!(!aborted.contains("NETPROBE drop reconnected"), "reconnected before the heal");
            let heal = inject(session, &source, "drop-heal", FaultAction::Heal { handle: HANDLE.into() }).await;
            assert_ne!(drop, heal);
            session
                .control(&source, "drop-finish", ControlCommand::SetRate { rate: PlaybackRate::Unlimited })
                .await;
            let source_witness = session.complete(&source, false, 1, SEED).await.unwrap();
            let oracle = session.stdout(&source, "oracle", false).await;
            let client = session.stdout(&source, "client", false).await;
            assert!(client.ends_with("NETPROBE drop done\n"), "client did not recover: {client}");
            assert!(client.contains("NETPROBE drop reconnected connects="), "{client}");
            let renewed = oracle
                .lines()
                .find_map(|line| line.strip_prefix(DROP_REINCARNATION))
                .expect("oracle observed the reincarnation");
            let (old, new) = renewed.split_once(" new=").unwrap();
            let (old, new): (u64, u64) = (old.parse().unwrap(), new.parse().unwrap());
            assert_eq!(old, connection.incarnation);
            assert!(new > old, "no newer incarnation: {renewed}");
            let source_applied = applied(session, &source).await;
            assert_eq!(
                source_applied,
                vec![(drop, "drop_connection".to_owned()), (heal, "heal".to_owned())]
            );
            // Replay the service-frozen spec, which carries both live inputs.
            let resource = session.client.describe(&source).await.unwrap();
            assert_eq!(resource.spec.program.len(), 2);
            let replay = session
                .submit(
                    SubmitRequest {
                        fault_profile: None,
                        metadata: Default::default(),
                        options: Default::default(),
                        request_id: "drop-connection-replay".into(),
                        spec: resource.spec,
                    },
                    1,
                )
                .await;
            session.worker(&replay).await;
            let replay_witness = session.complete(&replay, false, 1, SEED).await.unwrap();
            assert_eq!(source_witness, replay_witness, "raw native witnesses differ");
            for machine in ["oracle", "server", "client"] {
                assert_eq!(
                    session.stdout(&source, machine, false).await,
                    session.stdout(&replay, machine, false).await,
                    "{machine} output differs on replay"
                );
            }
            assert_eq!(applied(session, &replay).await, source_applied);
            let comparison = session.client.compare(&source, &replay).await.unwrap();
            assert_eq!(comparison.status, ComparisonStatus::Match);
            assert_eq!(comparison.witness_matches, Some(true));
            eprintln!(
                "drop-connection replay MATCH backend={} source={source} replay={replay} witness={source_witness} connection={line} incarnation old={old} new={new} applied={source_applied:?}",
                label(backend)
            );
        })
    })
    .await;
}
