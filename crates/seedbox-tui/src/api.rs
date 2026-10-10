//! The only place the console talks to the server. Every task reports back as
//! an [`Update`]; the UI never reads anything else.
use crate::model::{App, Request, Update};
use futures_util::{Stream, StreamExt};
use seedbox_api::{
    ControlCommand, ControlReceipt, ControlRequest, ControlState, InputSource, SimulationId,
    SimulationSummary, SubmitRequest,
};
use seedbox_client::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const LIST_EVERY: Duration = Duration::from_secs(1);
const WORLD_EVERY: Duration = Duration::from_millis(250);
const RECEIPT_WAIT: Duration = Duration::from_secs(2);
const LIST_PAGES: usize = 20;
const REOPEN_AFTER: Duration = Duration::from_millis(500);

pub struct Api {
    client: Client,
    updates: UnboundedSender<Update>,
    feeds: Vec<JoinHandle<()>>,
    list: JoinHandle<()>,
}

impl Api {
    pub fn new(client: Client, updates: UnboundedSender<Update>) -> Self {
        let list = tokio::spawn(poll_list(client.clone(), updates.clone()));
        Self {
            client,
            updates,
            feeds: Vec::new(),
            list,
        }
    }

    pub fn handle(&mut self, request: Request) {
        let client = self.client.clone();
        let tx = self.updates.clone();
        match request {
            Request::Open(id) => self.open(id),
            Request::RefreshWorld(id) => {
                tokio::spawn(async move {
                    if let Ok(world) = client.world(&id).await {
                        let _ = tx.send(Update::World(world));
                    }
                });
            }
            Request::Control(id, command) => {
                tokio::spawn(async move { control(client, tx, id, command).await });
            }
            Request::Rerun(id) => {
                tokio::spawn(async move {
                    let result = rerun(&client, &id).await;
                    let _ = tx.send(submitted(format!("rerun {id}"), result));
                });
            }
            Request::Branch(id) => {
                tokio::spawn(async move {
                    let result = branch(&client, &id).await;
                    let _ = tx.send(submitted(format!("branch {id}"), result));
                });
            }
            Request::Stop(id) => {
                tokio::spawn(async move {
                    let _ = tx.send(match client.stop(&id).await {
                        Ok(_) => Update::Notice(format!("stopping {id}")),
                        Err(e) => Update::Error(format!("stop {id}: {e}")),
                    });
                });
            }
            Request::Delete(id) => {
                tokio::spawn(async move {
                    let _ = tx.send(match client.delete(&id).await {
                        Ok(_) => Update::Notice(format!("deleting {id} and its branches")),
                        Err(e) => Update::Error(format!("delete {id}: {e}")),
                    });
                });
            }
            Request::Compare(baseline, id) => {
                tokio::spawn(async move {
                    let _ = tx.send(match client.compare(&baseline, &id).await {
                        Ok(comparison) => Update::Comparison(comparison),
                        Err(e) => Update::Error(format!("compare: {e}")),
                    });
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
                Update::Event,
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
                Update::Log,
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
                Update::Frames,
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
/// running yet, and reconnection starts at the current position, so keep
/// reopening until the simulation has completed.
async fn follow<S, T, O, F>(
    tx: UnboundedSender<Update>,
    done: Arc<AtomicBool>,
    what: &'static str,
    open: O,
    wrap: fn(T) -> Update,
) where
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
                            if tx.send(wrap(item)).is_err() {
                                return;
                            }
                        }
                        Err(e) if !reported => {
                            reported = true;
                            let _ = tx.send(Update::Error(format!("{what}: {e}")));
                        }
                        Err(_) => {}
                    }
                }
            }
            Err(e) if !reported => {
                reported = true;
                let _ = tx.send(Update::Error(format!("{what}: {e}")));
            }
            Err(_) => {}
        }
        if done.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(REOPEN_AFTER).await;
    }
}

async fn poll_list(client: Client, tx: UnboundedSender<Update>) {
    let mut tick = tokio::time::interval(LIST_EVERY);
    let mut reported = false;
    loop {
        tick.tick().await;
        match list_all(&client).await {
            Ok(list) => {
                reported = false;
                if tx.send(Update::List(list)).is_err() {
                    return;
                }
            }
            Err(e) if !reported => {
                reported = true;
                let _ = tx.send(Update::Error(format!("list: {e}")));
            }
            Err(_) => {}
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
async fn poll_focus(
    client: Client,
    tx: UnboundedSender<Update>,
    id: SimulationId,
    done: Arc<AtomicBool>,
) {
    let mut tick = tokio::time::interval(WORLD_EVERY);
    let mut round = 0u64;
    loop {
        tick.tick().await;
        if let Ok(world) = client.world(&id).await {
            if tx.send(Update::World(world)).is_err() {
                return;
            }
        }
        if let Ok(playback) = client.playback(&id).await {
            let _ = tx.send(Update::Playback(playback));
        }
        if round.is_multiple_of(4) {
            if let Ok(resource) = client.describe(&id).await {
                if resource.status.phase == seedbox_api::Phase::Completed {
                    done.store(true, Ordering::Relaxed);
                }
                let _ = tx.send(Update::Resource(resource));
            }
            if let Ok(receipts) = client.checkpoints(&id).await {
                let names = captured(&receipts)
                    .into_iter()
                    .map(|r| r.request.request_id.clone())
                    .collect();
                let _ = tx.send(Update::Checkpoints(names));
            }
        }
        round += 1;
    }
}

fn captured(receipts: &[ControlReceipt]) -> Vec<&ControlReceipt> {
    let mut out: Vec<&ControlReceipt> = receipts
        .iter()
        .filter(|r| match &r.state {
            ControlState::Acknowledged {
                checkpoint: Some(c),
                error: None,
                ..
            } => c.state == seedbox_engine_protocol::host_control::CheckpointState::Captured,
            _ => false,
        })
        .collect();
    out.sort_by_key(|r| match &r.state {
        ControlState::Acknowledged {
            checkpoint: Some(c),
            ..
        } => c.position.boundary_index,
        _ => 0,
    });
    out
}

fn command_title(command: &ControlCommand) -> String {
    match command {
        ControlCommand::SetRate { rate } => format!("rate {}", crate::model::rate_label(*rate)),
        ControlCommand::Step => "step".into(),
        ControlCommand::Checkpoint => "checkpoint".into(),
        ControlCommand::InjectFault { action } => serde_json::to_value(action)
            .ok()
            .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(str::to_owned))
            .map_or_else(|| "fault".into(), |a| format!("fault {a}")),
    }
}

async fn control(
    client: Client,
    tx: UnboundedSender<Update>,
    id: SimulationId,
    command: ControlCommand,
) {
    let title = command_title(&command);
    let request = match Client::request_id() {
        Ok(request_id) => ControlRequest {
            request_id,
            command,
        },
        Err(e) => {
            let _ = tx.send(Update::Error(e.to_string()));
            return;
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
    let _ = tx.send(Update::Receipt {
        title,
        request: shown,
        result,
    });
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

async fn submit(
    client: &Client,
    spec: seedbox_api::SimulationSpec,
) -> Result<(String, seedbox_api::Simulation), String> {
    let request = SubmitRequest {
        request_id: Client::request_id().map_err(|e| e.to_string())?,
        spec,
    };
    let shown = serde_json::to_string_pretty(&request).unwrap_or_default();
    let simulation = client.submit(&request).await.map_err(|e| e.to_string())?;
    Ok((shown, simulation))
}

async fn rerun(
    client: &Client,
    id: &SimulationId,
) -> Result<(String, seedbox_api::Simulation), String> {
    let resource = client.describe(id).await.map_err(|e| e.to_string())?;
    if !matches!(resource.spec.inputs, InputSource::Retained { .. }) {
        return Err(format!(
            "{id} has no prepared inputs; run its project again"
        ));
    }
    submit(client, resource.spec).await
}

/// Branch from the newest captured checkpoint.
async fn branch(
    client: &Client,
    id: &SimulationId,
) -> Result<(String, seedbox_api::Simulation), String> {
    let receipts = client.checkpoints(id).await.map_err(|e| e.to_string())?;
    let Some(newest) = captured(&receipts)
        .last()
        .map(|r| r.request.request_id.clone())
    else {
        return Err(format!("{id} has no captured checkpoint; press c first"));
    };
    let mut resource = client.describe(id).await.map_err(|e| e.to_string())?;
    resource.spec.inputs = InputSource::Checkpoint {
        simulation: id.clone(),
        checkpoint: newest,
    };
    submit(client, resource.spec).await
}

/// Drive the console until the user quits.
pub async fn drive(
    mut app: App,
    client: Client,
    terminal: &mut ratatui::DefaultTerminal,
) -> std::io::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
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
