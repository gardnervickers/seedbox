//! The only place the console talks to the server. Every task reports back as
//! an [`Update`]; the UI never reads anything else.
use crate::model::{App, Request, Update};
use futures_util::{Stream, StreamExt};
use seedbox_api::{
    ControlCommand, ControlReceipt, ControlRequest, ControlState, SimulationId, SimulationSummary,
};
use seedbox_client::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

// Bound pending stream updates as well as displayed logs. Awaiting capacity
// propagates backpressure to the HTTP reader; server stream gaps remain explicit.
const UPDATE_CAPACITY: usize = 256;

const LIST_EVERY: Duration = Duration::from_secs(1);
const WORLD_EVERY: Duration = Duration::from_millis(250);
const RECEIPT_WAIT: Duration = Duration::from_secs(2);
const LIST_PAGES: usize = 20;
const REOPEN_AFTER: Duration = Duration::from_millis(500);

pub struct Api {
    client: Client,
    updates: Sender<Update>,
    feeds: Vec<JoinHandle<()>>,
    list: JoinHandle<()>,
    /// One world refresh at a time; event bursts collapse into it.
    refreshing: Arc<AtomicBool>,
}

impl Api {
    pub fn new(client: Client, updates: Sender<Update>) -> Self {
        let list = tokio::spawn(poll_list(client.clone(), updates.clone()));
        Self {
            client,
            updates,
            feeds: Vec::new(),
            list,
            refreshing: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn handle(&mut self, request: Request) {
        let client = self.client.clone();
        let tx = self.updates.clone();
        match request {
            Request::Open(id) => self.open(id),
            Request::Close => {
                for feed in self.feeds.drain(..) {
                    feed.abort();
                }
            }
            Request::RefreshWorld(id) => {
                if self.refreshing.swap(true, Ordering::AcqRel) {
                    return;
                }
                let refreshing = self.refreshing.clone();
                tokio::spawn(async move {
                    let world = client.world(&id).await;
                    refreshing.store(false, Ordering::Release);
                    if let Ok(world) = world {
                        let _ = tx.send(Update::World(world)).await;
                    }
                });
            }
            Request::Preview(id) => {
                tokio::spawn(async move {
                    let world = client.world(&id).await.ok().map(Box::new);
                    let timeline = client
                        .timeline(&id)
                        .await
                        .map(|t| t.entries)
                        .unwrap_or_default();
                    let _ = tx.send(Update::Preview(id, world, timeline)).await;
                });
            }
            Request::Heal(id, action) => {
                tokio::spawn(async move {
                    let handle = action["handle"].as_str().unwrap_or("fault").to_owned();
                    let update = match fault(client, tx.clone(), id.clone(), action).await {
                        FaultOutcome::Admitted => Update::Notice(format!("heal {handle} admitted")),
                        FaultOutcome::Rejected(error) => {
                            Update::Error(format!("heal {handle} rejected: {error}"))
                        }
                        FaultOutcome::Uncertain => Update::Error(format!(
                            "heal {handle} not acknowledged yet; watch the faults panel"
                        )),
                    };
                    let _ = tx.send(Update::Focused(id, Box::new(update))).await;
                });
            }
            Request::BeginFaultMenu(id) => {
                tokio::spawn(async move {
                    let result = begin_fault_menu(&client, &tx, &id).await;
                    let _ = tx
                        .send(Update::Focused(
                            id,
                            Box::new(Update::FaultMenuReady(result)),
                        ))
                        .await;
                });
            }
            Request::EndFaultMenu(id, rate) => {
                tokio::spawn(async move {
                    end_fault_menu(client, tx, id, rate).await;
                });
            }
            Request::Fault(id, action, rate) => {
                tokio::spawn(async move {
                    let outcome = fault(client.clone(), tx.clone(), id.clone(), action).await;
                    let update = finish_fault(outcome, rate, || {
                        restore_menu_rate(client, tx.clone(), id.clone(), rate)
                    })
                    .await;
                    let _ = tx.send(Update::Focused(id, Box::new(update))).await;
                });
            }
            Request::Control(id, command) => {
                tokio::spawn(async move {
                    control(client, tx.clone(), id.clone(), command).await;
                    let _ = tx
                        .send(Update::Focused(id, Box::new(Update::ControlDone)))
                        .await;
                });
            }
            Request::PauseThenStep(id) => {
                tokio::spawn(async move {
                    pause_then_step(|command| {
                        control(client.clone(), tx.clone(), id.clone(), command)
                    })
                    .await;
                    let _ = tx
                        .send(Update::Focused(id, Box::new(Update::ControlDone)))
                        .await;
                });
            }
            Request::Rerun(id) => {
                tokio::spawn(async move {
                    let result = rerun(&client, &id).await;
                    let _ = tx.send(submitted(format!("rerun {id}"), result)).await;
                });
            }
            Request::SubmitDefinition(document) => {
                tokio::spawn(async move {
                    let shown = serde_json::to_string_pretty(&document).unwrap_or_default();
                    let result = client
                        .submit_definition(&document, true)
                        .await
                        .map(|simulation| (shown, simulation))
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(submitted("run edited simulation".into(), result))
                        .await;
                });
            }
            Request::Branch(id) => {
                tokio::spawn(async move {
                    let result = branch(&client, &id).await;
                    let _ = tx.send(submitted(format!("branch {id}"), result)).await;
                });
            }
            Request::Stop(id) => {
                tokio::spawn(async move {
                    let _ = tx
                        .send(match client.stop(&id).await {
                            Ok(_) => Update::Notice(format!("stopping {id}")),
                            Err(e) => Update::Error(format!("stop {id}: {e}")),
                        })
                        .await;
                });
            }
            Request::Delete(id) => {
                tokio::spawn(async move {
                    let _ = tx
                        .send(match client.delete(&id).await {
                            Ok(_) => Update::Notice(format!("deleting {id} and its branches")),
                            Err(e) => Update::Error(format!("delete {id}: {e}")),
                        })
                        .await;
                });
            }
            Request::Compare(baseline, id) => {
                tokio::spawn(async move {
                    let _ = tx
                        .send(match client.compare(&baseline, &id).await {
                            Ok(comparison) => Update::Comparison(comparison),
                            Err(e) => Update::Error(format!("compare: {e}")),
                        })
                        .await;
                });
            }
        }
    }

    /// Replace every per-simulation feed with feeds for `id`.
    fn open(&mut self, id: SimulationId) {
        for feed in self.feeds.drain(..) {
            feed.abort();
        }
        let done = Arc::new(AtomicBool::new(false));
        let c = &self.client;
        let tx = &self.updates;
        self.feeds = vec![
            tokio::spawn(poll_focus(c.clone(), tx.clone(), id.clone(), done.clone())),
            tokio::spawn(follow(
                tx.clone(),
                done.clone(),
                "events",
                {
                    let (c, id) = (c.clone(), id.clone());
                    move || {
                        let (c, id) = (c.clone(), id.clone());
                        async move { c.events(&id).await }
                    }
                },
                {
                    let id = id.clone();
                    move |delivery| Update::Focused(id.clone(), Box::new(Update::Event(delivery)))
                },
            )),
            tokio::spawn(follow(
                tx.clone(),
                done.clone(),
                "logs",
                {
                    let (c, id) = (c.clone(), id.clone());
                    move || {
                        let (c, id) = (c.clone(), id.clone());
                        async move { c.all_logs(&id).await }
                    }
                },
                {
                    let id = id.clone();
                    move |delivery| Update::Log(id.clone(), delivery)
                },
            )),
            tokio::spawn(follow(
                tx.clone(),
                done.clone(),
                "frames",
                {
                    let (c, id) = (c.clone(), id.clone());
                    move || {
                        let (c, id) = (c.clone(), id.clone());
                        async move { c.frames(&id).await }
                    }
                },
                {
                    let id = id.clone();
                    move |delivery| Update::Focused(id.clone(), Box::new(Update::Frames(delivery)))
                },
            )),
        ];
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.list.abort();
        for feed in &self.feeds {
            feed.abort();
        }
    }
}

/// Forward a server stream. Live streams close when a simulation is not
/// running yet, so keep reopening until the simulation has completed. Log
/// streams replay bounded history; the model discards already-seen sequences.
async fn follow<S, T, O, F, W>(
    tx: Sender<Update>,
    done: Arc<AtomicBool>,
    what: &'static str,
    open: O,
    wrap: W,
) where
    W: Fn(T) -> Update,
    O: Fn() -> F,
    F: std::future::Future<Output = Result<S, seedbox_client::Error>>,
    S: Stream<Item = Result<T, seedbox_client::Error>> + Unpin,
{
    let mut reported = false;
    loop {
        match open().await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(item) => {
                            reported = false;
                            if tx.send(wrap(item)).await.is_err() {
                                return;
                            }
                        }
                        Err(e) if !reported => {
                            reported = true;
                            let _ = tx.send(Update::Error(format!("{what}: {e}"))).await;
                        }
                        Err(_) => {}
                    }
                }
            }
            Err(e) if !reported => {
                reported = true;
                let _ = tx.send(Update::Error(format!("{what}: {e}"))).await;
            }
            Err(_) => {}
        }
        if done.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(REOPEN_AFTER).await;
    }
}

async fn poll_list(client: Client, tx: Sender<Update>) {
    let mut tick = tokio::time::interval(LIST_EVERY);
    let mut reported = false;
    loop {
        tick.tick().await;
        match list_all(&client).await {
            Ok(list) => {
                reported = false;
                let _ = tx.send(Update::Connection(true)).await;
                if tx.send(Update::List(list)).await.is_err() {
                    return;
                }
            }
            Err(_) => {
                if !reported {
                    reported = true;
                    let _ = tx.send(Update::Connection(false)).await;
                }
            }
        }
    }
}

async fn list_all(client: &Client) -> Result<Vec<SimulationSummary>, seedbox_client::Error> {
    let mut items = Vec::new();
    let mut after = None;
    for _ in 0..LIST_PAGES {
        let page = client.list(after.as_ref()).await?;
        items.extend(page.items);
        match page.next_cursor {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(items)
}

/// The world and playback are documents, not streams: poll them so the
/// screen follows machines and links as the server reports them.
async fn poll_focus(client: Client, tx: Sender<Update>, id: SimulationId, done: Arc<AtomicBool>) {
    let mut tick = tokio::time::interval(WORLD_EVERY);
    let mut round = 0u64;
    let mut catalogue_error = false;
    loop {
        tick.tick().await;
        if let Ok(world) = client.world(&id).await {
            if tx.send(Update::World(world)).await.is_err() {
                return;
            }
        }
        if let Ok(playback) = client.playback(&id).await {
            let _ = tx
                .send(Update::Focused(
                    id.clone(),
                    Box::new(Update::Playback(playback)),
                ))
                .await;
        }
        if round.is_multiple_of(4) {
            match client.fault_types(&id).await {
                Ok(catalogue) => {
                    catalogue_error = false;
                    let _ = tx
                        .send(Update::Focused(
                            id.clone(),
                            Box::new(Update::FaultTypes(catalogue)),
                        ))
                        .await;
                }
                Err(error) if !catalogue_error => {
                    catalogue_error = true;
                    let _ = tx
                        .send(Update::Focused(
                            id.clone(),
                            Box::new(Update::Error(format!("fault catalogue: {error}"))),
                        ))
                        .await;
                }
                Err(_) => {}
            }
            match client.describe(&id).await {
                Ok(resource) => {
                    if resource.status.phase == seedbox_api::Phase::Completed {
                        done.store(true, Ordering::Relaxed);
                    }
                    let _ = tx.send(Update::Resource(resource)).await;
                }
                Err(seedbox_client::Error::Rejected {
                    code: seedbox_api::ErrorCode::NotFound,
                    ..
                }) => {
                    done.store(true, Ordering::Relaxed);
                    let _ = tx.send(Update::Missing(id.clone())).await;
                    return;
                }
                Err(_) => {}
            }
            match client.timeline(&id).await {
                Ok(timeline) => {
                    let _ = tx
                        .send(Update::Focused(
                            id.clone(),
                            Box::new(Update::Timeline(timeline)),
                        ))
                        .await;
                }
                Err(error) => {
                    let _ = tx
                        .send(Update::Focused(
                            id.clone(),
                            Box::new(Update::Error(format!("timeline: {error}"))),
                        ))
                        .await;
                }
            }
            if let Ok(receipts) = client.checkpoints(&id).await {
                let checkpoints = captured(&receipts)
                    .into_iter()
                    .map(|(receipt, position)| crate::model::Checkpoint {
                        name: receipt.request.request_id.clone(),
                        at_ns: position.simulated_time_ns,
                    })
                    .collect();
                let _ = tx
                    .send(Update::Focused(
                        id.clone(),
                        Box::new(Update::Checkpoints(checkpoints)),
                    ))
                    .await;
            }
        }
        round += 1;
    }
}

/// Captured checkpoints with their committed positions, oldest first.
fn captured(receipts: &[ControlReceipt]) -> Vec<(&ControlReceipt, seedbox_api::BoundaryPosition)> {
    let mut out: Vec<_> = receipts
        .iter()
        .filter_map(|r| match &r.state {
            ControlState::Acknowledged {
                checkpoint: Some(c),
                error: None,
                ..
            } if c.state == seedbox_api::CheckpointState::Captured => Some((r, c.position)),
            _ => None,
        })
        .collect();
    out.sort_by_key(|(_, position)| position.boundary_index);
    out
}

fn command_title(command: &ControlCommand) -> String {
    match command {
        ControlCommand::SetRate { rate } => format!("rate {}", crate::model::rate_label(*rate)),
        ControlCommand::Step => "step".into(),
        ControlCommand::PauseBeforeNextFault { .. } => "pause before next fault".into(),
        ControlCommand::Checkpoint => "checkpoint".into(),
        ControlCommand::InjectFault { action } => serde_json::to_value(action)
            .ok()
            .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(str::to_owned))
            .map_or_else(|| "fault".into(), |a| format!("fault {a}")),
    }
}

/// A step requires the pause to have reached the worker, not merely to have
/// been accepted by the server. Failed or still-pending pauses never send step.
async fn pause_then_step<F, Fut>(mut send: F)
where
    F: FnMut(ControlCommand) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if send(ControlCommand::SetRate {
        rate: seedbox_api::PlaybackRate::Paused,
    })
    .await
    {
        send(ControlCommand::Step).await;
    }
}

async fn control(
    client: Client,
    tx: Sender<Update>,
    id: SimulationId,
    command: ControlCommand,
) -> bool {
    let title = command_title(&command);
    let request = match Client::request_id() {
        Ok(request_id) => ControlRequest {
            request_id,
            command,
        },
        Err(e) => {
            let request = ControlRequest {
                request_id: String::new(),
                command,
            };
            let _ = tx
                .send(Update::Focused(
                    id,
                    Box::new(Update::Receipt {
                        title,
                        request: serde_json::to_string(&request).unwrap_or_default(),
                        result: Err(e.to_string()),
                    }),
                ))
                .await;
            return false;
        }
    };
    let shown = serde_json::to_string_pretty(&request).unwrap_or_default();
    let result = async {
        let mut receipt = client.request_control(&id, &request).await?;
        let deadline = tokio::time::Instant::now() + RECEIPT_WAIT;
        while receipt.state == ControlState::Accepted && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            receipt = client.control_receipt(&id, &request.request_id).await?;
        }
        Ok::<_, seedbox_client::Error>(receipt)
    }
    .await
    .map_err(|e| e.to_string());
    let acknowledged = result.as_ref().is_ok_and(|receipt| {
        matches!(
            &receipt.state,
            ControlState::Acknowledged { error: None, .. }
        )
    });
    let _ = tx
        .send(Update::Focused(
            id,
            Box::new(Update::Receipt {
                title,
                request: shown,
                result,
            }),
        ))
        .await;
    acknowledged
}

/// Read the current rate from the server, then wait for a committed pause.
async fn begin_fault_menu(
    client: &Client,
    tx: &Sender<Update>,
    id: &SimulationId,
) -> Result<seedbox_api::PlaybackRate, String> {
    let observed = client
        .playback(id)
        .await
        .map_err(|e| format!("cannot pause for fault menu: {e}"))?;
    if observed.state.terminal || !observed.state.application_ready || observed.state.stepping {
        return Err(
            "fault menu requires a ready, nonterminal simulation with no step in progress".into(),
        );
    }
    let rate = observed.state.rate;
    if control(
        client.clone(),
        tx.clone(),
        id.clone(),
        ControlCommand::SetRate {
            rate: seedbox_api::PlaybackRate::Paused,
        },
    )
    .await
    {
        Ok(rate)
    } else {
        Err("pause was not acknowledged; fault menu was not opened. Playback may be paused; Esc to inspect controls".into())
    }
}

async fn end_fault_menu(
    client: Client,
    tx: Sender<Update>,
    id: SimulationId,
    rate: seedbox_api::PlaybackRate,
) {
    let ok = restore_menu_rate(client, tx.clone(), id.clone(), rate).await;
    let _ = tx
        .send(Update::Focused(id, Box::new(Update::FaultMenuDone(ok))))
        .await;
}

async fn restore_menu_rate(
    client: Client,
    tx: Sender<Update>,
    id: SimulationId,
    rate: seedbox_api::PlaybackRate,
) -> bool {
    rate == seedbox_api::PlaybackRate::Paused
        || control(client, tx, id, ControlCommand::SetRate { rate }).await
}

enum FaultOutcome {
    Admitted,
    Rejected(String),
    Uncertain,
}

/// Restore the menu's rate only once the fault was admitted. A rejection keeps
/// the form open for correction; an uncertain outcome leaves playback paused.
async fn finish_fault<R>(
    outcome: FaultOutcome,
    rate: seedbox_api::PlaybackRate,
    restore: impl FnOnce() -> R,
) -> Update
where
    R: std::future::Future<Output = bool>,
{
    match outcome {
        FaultOutcome::Admitted => Update::FaultMenuDone(restore().await),
        FaultOutcome::Rejected(error) => Update::FaultRejected(rate, error),
        FaultOutcome::Uncertain => Update::FaultMenuDone(false),
    }
}

/// Fault forms pass the server's action document through unchanged.
async fn fault(
    client: Client,
    tx: Sender<Update>,
    id: SimulationId,
    action: serde_json::Value,
) -> FaultOutcome {
    let title = format!("fault {}", action["action"].as_str().unwrap_or("request"));
    let request_id = match Client::request_id() {
        Ok(id) => id,
        Err(error) => {
            let _ = tx
                .send(Update::Focused(
                    id,
                    Box::new(Update::FaultReceipt {
                        title,
                        request: action.to_string(),
                        result: Err(error.to_string()),
                    }),
                ))
                .await;
            return FaultOutcome::Rejected(error.to_string());
        }
    };
    let request = serde_json::json!({"requestId": request_id, "command": {"type":"injectFault", "action":action}});
    let shown = serde_json::to_string_pretty(&request).unwrap_or_default();
    let mut rejected = false;
    let result = async {
        let mut receipt = client
            .request_control_json(&id, &request)
            .await
            .map_err(|error| {
                rejected = matches!(
                    error,
                    seedbox_client::Error::Rejected {
                        code: seedbox_api::ErrorCode::InvalidRequest
                            | seedbox_api::ErrorCode::NotFound
                            | seedbox_api::ErrorCode::Conflict,
                        ..
                    }
                );
                error
            })?;
        let deadline = tokio::time::Instant::now() + RECEIPT_WAIT;
        while receipt["state"]["type"] == "accepted" && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            receipt = client.control_receipt_json(&id, &request_id).await?;
        }
        Ok::<_, seedbox_client::Error>(receipt)
    }
    .await
    .map_err(|error| error.to_string());
    let outcome = match &result {
        Ok(receipt) if fault_admitted(receipt) => FaultOutcome::Admitted,
        Ok(receipt)
            if receipt["state"]["type"] == "unsupported"
                || (receipt["state"]["type"] == "acknowledged"
                    && !receipt["state"]["error"].is_null()) =>
        {
            FaultOutcome::Rejected(format!(
                "{}: {}",
                receipt["state"]["error"],
                receipt["state"]["detail"]
                    .as_str()
                    .unwrap_or("fault rejected; correct the fields and retry")
            ))
        }
        Err(error) if rejected => FaultOutcome::Rejected(error.clone()),
        _ => FaultOutcome::Uncertain,
    };
    let _ = tx
        .send(Update::Focused(
            id,
            Box::new(Update::FaultReceipt {
                title,
                request: shown,
                result,
            }),
        ))
        .await;
    outcome
}

fn fault_admitted(receipt: &serde_json::Value) -> bool {
    let state = &receipt["state"];
    state["type"] == "acknowledged" && state["error"].is_null() && state["intervention"].is_object()
}

fn submitted(title: String, result: Result<(String, seedbox_api::Simulation), String>) -> Update {
    match result {
        Ok((request, simulation)) => Update::Submitted {
            title,
            request,
            result: Ok(simulation),
        },
        Err(error) => Update::Submitted {
            title,
            request: String::new(),
            result: Err(error),
        },
    }
}

async fn rerun(
    client: &Client,
    id: &SimulationId,
) -> Result<(String, seedbox_api::Simulation), String> {
    let resource = client.describe(id).await.map_err(|e| e.to_string())?;
    let mut document = serde_json::to_value(&resource).map_err(|e| e.to_string())?;
    annotate(&mut document, id, "rerun");
    if document["spec"].get("architecture").is_none() {
        return Err(format!(
            "{id} has no complete prepared definition; run its project again"
        ));
    }
    let shown = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?;
    let simulation = client
        .submit_definition(&document, false)
        .await
        .map_err(|e| e.to_string())?;
    Ok((shown, simulation))
}

/// Record lineage so the catalogue can nest a derived run under its source.
pub(crate) fn annotate(document: &mut serde_json::Value, source: &SimulationId, how: &str) {
    let annotations = &mut document["metadata"]["annotations"];
    if !annotations.is_object() {
        *annotations = serde_json::json!({});
    }
    annotations["derived-from"] = source.to_string().into();
    annotations["derivation"] = how.into();
}

/// Branch from the newest captured checkpoint.
async fn branch(
    client: &Client,
    id: &SimulationId,
) -> Result<(String, seedbox_api::Simulation), String> {
    let receipts = client.checkpoints(id).await.map_err(|e| e.to_string())?;
    let Some((newest, position)) = captured(&receipts)
        .last()
        .map(|(r, position)| (r.request.request_id.clone(), *position))
    else {
        return Err(format!("{id} has no captured checkpoint; press c first"));
    };
    let resource = client
        .checkpoint_definition(id, &newest)
        .await
        .map_err(|e| e.to_string())?;
    let mut document = serde_json::to_value(resource).map_err(|e| e.to_string())?;
    annotate(&mut document, id, "branch");
    document["metadata"]["annotations"]["branch-at-ns"] =
        position.simulated_time_ns.to_string().into();
    let shown = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?;
    let simulation = client
        .submit_definition(&document, false)
        .await
        .map_err(|e| e.to_string())?;
    Ok((shown, simulation))
}

/// Drive the console until the user quits.
pub async fn drive(
    mut app: App,
    client: Client,
    terminal: &mut ratatui::DefaultTerminal,
) -> std::io::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(UPDATE_CAPACITY);
    let mut api = Api::new(client, tx);
    if let Some(focus) = &app.focus {
        api.handle(Request::Open(focus.id.clone()));
    }
    let mut input = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !app.quit {
        let requests = tokio::select! {
            event = input.next() => match event {
                Some(Ok(event)) => app.input(event),
                Some(Err(e)) => return Err(e),
                None => break,
            },
            Some(update) = rx.recv() => app.apply(update),
            _ = tick.tick() => {
                if let Some(focus) = app.focus.as_mut() {
                    let now = focus.now_ns(std::time::Instant::now());
                    focus.prune_frames(now);
                }
                terminal.draw(|frame| crate::view::draw(frame, &app))?;
                Vec::new()
            }
        };
        for request in requests {
            api.handle(request);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[tokio::test]
    async fn slow_ui_backpressures_stream_without_losing_update_order() {
        use std::sync::atomic::AtomicUsize;
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let read = Arc::new(AtomicUsize::new(0));
        let observed = read.clone();
        let task = tokio::spawn(follow(
            tx,
            Arc::new(AtomicBool::new(true)),
            "test",
            move || {
                let read = read.clone();
                async move {
                    Ok(
                        futures_util::stream::iter((0..100).map(Ok)).inspect(move |_| {
                            read.fetch_add(1, Ordering::Relaxed);
                        }),
                    )
                }
            },
            |value| Update::Notice(format!("{value}")),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while observed.load(Ordering::Relaxed) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Two queued updates and one awaiting capacity; the rest stays unread.
        assert_eq!(observed.load(Ordering::Relaxed), 3);
        for expected in 0..100 {
            let update = rx.recv().await.unwrap();
            assert!(matches!(update, Update::Notice(value) if value == expected.to_string()));
        }
        task.await.unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn fault_restores_rate_only_after_admission() {
        let rate = seedbox_api::PlaybackRate::Unlimited;
        let calls = std::cell::Cell::new(0);
        let restore = |result| {
            let calls = &calls;
            move || async move {
                calls.set(calls.get() + 1);
                result
            }
        };
        assert!(matches!(
            finish_fault(FaultOutcome::Admitted, rate, restore(true)).await,
            Update::FaultMenuDone(true)
        ));
        assert!(matches!(
            finish_fault(FaultOutcome::Admitted, rate, restore(false)).await,
            Update::FaultMenuDone(false)
        ));
        assert_eq!(calls.get(), 2);
        assert!(matches!(
            finish_fault(FaultOutcome::Rejected("bad".into()), rate, restore(true)).await,
            Update::FaultRejected(r, e) if r == rate && e == "bad"
        ));
        assert!(matches!(
            finish_fault(FaultOutcome::Uncertain, rate, restore(true)).await,
            Update::FaultMenuDone(false)
        ));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn only_acknowledged_fault_admission_allows_restore() {
        for state in ["accepted", "unsupported", "interrupted"] {
            assert!(!fault_admitted(
                &serde_json::json!({"state":{"type":state}})
            ));
        }
        assert!(!fault_admitted(
            &serde_json::json!({"state":{"type":"acknowledged","error":"invalid"}})
        ));
        assert!(!fault_admitted(
            &serde_json::json!({"state":{"type":"acknowledged"}})
        ));
        assert!(fault_admitted(
            &serde_json::json!({"state":{"type":"acknowledged","intervention":{"plan":{"id":1}}}})
        ));
    }

    #[tokio::test]
    async fn step_waits_for_successful_pause_acknowledgement() {
        let commands = RefCell::new(Vec::new());
        let acknowledged = std::cell::Cell::new(false);
        pause_then_step(|command| {
            let commands = &commands;
            let acknowledged = &acknowledged;
            async move {
                match command {
                    ControlCommand::SetRate {
                        rate: seedbox_api::PlaybackRate::Paused,
                    } => {
                        commands.borrow_mut().push(command);
                        tokio::task::yield_now().await;
                        acknowledged.set(true);
                    }
                    ControlCommand::Step => {
                        assert!(acknowledged.get());
                        commands.borrow_mut().push(command);
                    }
                    _ => panic!("unexpected command"),
                }
                true
            }
        })
        .await;
        assert_eq!(
            *commands.borrow(),
            [
                ControlCommand::SetRate {
                    rate: seedbox_api::PlaybackRate::Paused
                },
                ControlCommand::Step,
            ]
        );
    }

    #[tokio::test]
    async fn failed_or_pending_pause_does_not_submit_step() {
        let commands = RefCell::new(Vec::new());
        pause_then_step(|command| {
            commands.borrow_mut().push(command);
            std::future::ready(false)
        })
        .await;
        assert_eq!(
            *commands.borrow(),
            [ControlCommand::SetRate {
                rate: seedbox_api::PlaybackRate::Paused
            }]
        );
    }
    fn timeline_server(
        responses: Vec<seedbox_api::Simulation>,
    ) -> (Client, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let path = std::path::PathBuf::from("/tmp")
            .join(format!("sbtui-{}.sock", Client::request_id().unwrap()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = Client::connect_to(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "expected timeline HTTP request"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let body_offset = loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        break offset + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..body_offset]).to_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < body_offset + length {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let body = serde_json::to_vec(&response).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
            std::fs::remove_file(path).unwrap();
            requests
        });
        (client, worker)
    }

    #[tokio::test]
    async fn full_definition_submission_needs_one_post_and_preserves_metadata_and_options() {
        let mut resource = crate::model::tests::timeline_resource(10);
        let asset = format!("sha256:{}", "a".repeat(64));
        resource.spec = serde_json::from_value(serde_json::json!({
            "architecture":"aarch64", "seed":7, "clock":{"epochSeconds":86400},
            "runtime":{"kernel":asset,"engine":asset,"execution":{}},
            "machines":{}, "oracle":{"bootArchive":asset,"memoryBytes":1073741824_u64,
                "process":{"argv":["/test"],"environment":{},"workingDirectory":"/","user":"0"},"volumes":{}},
            "network":{"name":"default","latency_ns":1000000,"jitter_ns":0},
            "simulator":{"cpu":{"speed":"2GHz"},"schedule":{"timer_jitter_ns":0,"stop_tolerance_ns":0},
                "disk":{"torn_writes":false},"oracleControl":false},
            "start":{"kind":"fresh"},
            "overrides":[{"id":"external","atNs":42,"action":{"action":"future"}}]
        })).unwrap();
        resource.metadata.name = "saved experiment".into();
        resource
            .metadata
            .labels
            .insert("application".into(), "demo".into());
        resource.options.wall_time_ns = Some(9999);
        let document = serde_json::to_value(&resource).unwrap();
        let (client, worker) = timeline_server(vec![resource]);
        client.submit_definition(&document, true).await.unwrap();
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /v1/simulations "));
        let body = requests[0].split_once("\r\n\r\n").unwrap().1;
        let request: seedbox_api::SubmitRequest = serde_json::from_str(body).unwrap();
        assert_eq!(request.metadata.name, "saved experiment");
        assert_eq!(request.metadata.labels["application"], "demo");
        assert!(request.options.start_paused);
        assert_eq!(request.options.wall_time_ns, Some(9999));
        assert_eq!(request.spec.program[0].key, "external");
        assert!(matches!(
            request.spec.inputs,
            seedbox_api::InputSource::Resolved { .. }
        ));
        assert!(!request.request_id.is_empty());
    }

    #[tokio::test]
    async fn source_dependent_resource_cannot_be_resubmitted_as_definition() {
        let resource = crate::model::tests::timeline_resource(10);
        let (client, worker) = timeline_server(vec![resource]);
        let error = rerun(&client, &"k7m2p9qx".parse().unwrap())
            .await
            .unwrap_err();
        assert!(error.contains("complete prepared definition"));
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /v1/simulations/k7m2p9qx "));
    }
}
