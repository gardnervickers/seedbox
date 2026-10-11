//! Client-side state. Every machine, link and fault shown comes from the API;
//! nothing here knows a world's shape in advance.
use crate::activity::{Activity, Change, Key, Transition};
use crate::fault_form::{FaultForm, FormOutcome};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use seedbox_api::{
    ControlCommand, ControlReceipt, EventDelivery, EventKind, FaultCatalogue, Frame, FrameDelivery,
    FrameFate, LogDelivery, LogEntry, LogStream, PlaybackObservation, PlaybackRate, Simulation,
    SimulationComparison, SimulationEvent, SimulationId, SimulationSummary, WorldState,
};
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

pub(super) fn simulation_name(resource: &Simulation) -> &str {
    if resource.metadata.name.is_empty() {
        resource.metadata.id.as_str()
    } else {
        &resource.metadata.name
    }
}

const MAX_EVENTS: usize = 500;
const MAX_LOG_LINES: usize = 1000;
const MAX_FRAMES: usize = 4000;
const NOTICE_FOR: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Catalogue,
    Simulation,
    Timeline,
}

/// Everything the API tasks report back to the UI.
#[derive(Debug)]
pub enum Update {
    Focused(SimulationId, Box<Update>),
    Missing(SimulationId),
    List(Vec<SimulationSummary>),
    Resource(Simulation),
    World(WorldState),
    FaultTypes(FaultCatalogue),
    Timeline(seedbox_api::FaultTimeline),
    FaultReceipt {
        title: String,
        request: String,
        result: Result<serde_json::Value, String>,
    },
    Playback(PlaybackObservation),
    FaultMenuReady(Result<PlaybackRate, String>),
    FaultMenuDone(bool),
    FaultRejected(PlaybackRate, String),
    ControlDone,
    Event(EventDelivery),
    Log(SimulationId, LogDelivery),
    Frames(FrameDelivery),
    Receipt {
        title: String,
        request: String,
        result: Result<ControlReceipt, String>,
    },
    Checkpoints(Vec<Checkpoint>),
    /// Whether the catalogue poll reached the server.
    Connection(bool),
    /// Faults and timeline of a catalogue row, for its detail pane.
    Preview(
        SimulationId,
        Option<Box<WorldState>>,
        Vec<seedbox_api::TimelineEntry>,
    ),
    Comparison(SimulationComparison),
    Submitted {
        title: String,
        request: String,
        result: Result<Simulation, String>,
    },
    Notice(String),
    Error(String),
}

/// What the UI asks the API layer to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Open(SimulationId),
    Close,
    Control(SimulationId, ControlCommand),
    BeginFaultMenu(SimulationId),
    EndFaultMenu(SimulationId, PlaybackRate),
    Fault(SimulationId, serde_json::Value, PlaybackRate),
    PauseThenStep(SimulationId),
    Rerun(SimulationId),
    SubmitDefinition(serde_json::Value),
    Branch(SimulationId),
    Stop(SimulationId),
    Delete(SimulationId),
    Compare(SimulationId, SimulationId),
    RefreshWorld(SimulationId),
    Preview(SimulationId),
    /// Inject a heal without pausing; heals are admitted at the next boundary.
    Heal(SimulationId, serde_json::Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Good,
    Bad,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub tone: Tone,
    pub at: Instant,
}

/// The newest request and its result, shown in place of the log pane.
#[derive(Clone, Debug)]
pub struct Inspector {
    pub title: String,
    pub request: String,
    pub result: String,
    pub ok: bool,
    pub at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub time_ns: u64,
    pub stream: LogStream,
    pub text: String,
}

#[derive(Default)]
pub struct MachineLog {
    pub lines: VecDeque<LogLine>,
    partial: Vec<u8>,
}

impl MachineLog {
    fn push(&mut self, time_ns: u64, stream: LogStream, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            self.push_line(time_ns, stream, text);
        }
        if self.partial.len() > 4096 {
            let text = String::from_utf8_lossy(&self.partial).to_string();
            self.partial.clear();
            self.push_line(time_ns, stream, text);
        }
    }

    fn note(&mut self, time_ns: u64, text: String) {
        self.partial.clear();
        self.push_line(time_ns, LogStream::Stderr, text);
    }

    fn push_line(&mut self, time_ns: u64, stream: LogStream, text: String) {
        self.lines.push_back(LogLine {
            time_ns,
            stream,
            text,
        });
        if self.lines.len() > MAX_LOG_LINES {
            self.lines.pop_front();
        }
    }
}

/// One simulation opened for inspection.
pub struct Focus {
    pub id: SimulationId,
    pub resource: Option<Simulation>,
    pub fault_types: Option<FaultCatalogue>,
    pub world: Option<WorldState>,
    pub playback: Option<(PlaybackObservation, Instant)>,
    pub frames: VecDeque<Frame>,
    pub frame_time_ns: u64,
    pub events: VecDeque<SimulationEvent>,
    pub logs: BTreeMap<String, MachineLog>,
    log_sequence: u64,
    /// Newest event delivery applied; reopened streams replay history.
    event_sequence: u64,
    pending_heals: BTreeMap<String, Option<u32>>,
    pub selected: Option<String>,
    pub checkpoints: Vec<Checkpoint>,
    /// The rate to resume at after a pause.
    pub resume_rate: PlaybackRate,
    pub lost: u64,
    /// The server's fault timeline for this simulation, newest observation.
    pub timeline: Vec<seedbox_api::TimelineEntry>,
    /// Machine transitions seen on the live event stream.
    pub transitions: Vec<Transition>,
    pub traffic: Traffic,
    /// Machines in the order first observed, so the map keeps each machine
    /// in place while others come and go.
    pub seen: Vec<String>,
    pub fault_selected: Option<Key>,
    /// Per-machine preparation progress reported before execution.
    pub preparation: BTreeMap<String, seedbox_api::PreparationStage>,
}

/// What the catalogue knows about a simulation beyond its summary.
pub struct Preview {
    pub world: Option<WorldState>,
    pub timeline: Vec<seedbox_api::TimelineEntry>,
    fetched: Instant,
}

const PREVIEW_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub name: String,
    pub at_ns: u64,
}

/// Simulated-time traffic rates derived from successive world counters.
#[derive(Default)]
pub struct Traffic {
    baseline: Option<(u64, LinkCounts)>,
    /// Frames per simulated second on each directed link.
    pub links: BTreeMap<(String, String), f64>,
    /// Recent transmit rates per machine, oldest first.
    pub sent: BTreeMap<String, VecDeque<f64>>,
    pub received: BTreeMap<String, f64>,
}

/// Scheduled-frame counters per directed link.
type LinkCounts = BTreeMap<(String, String), u64>;

const TRAFFIC_WINDOW_NS: u64 = 100_000_000;
const TRAFFIC_SAMPLES: usize = 24;
const MAX_TRANSITIONS: usize = 2000;

impl Traffic {
    fn observe(&mut self, world: &WorldState) {
        let counts: BTreeMap<(String, String), u64> = world
            .links
            .iter()
            .map(|l| ((l.from.clone(), l.to.clone()), l.counters.scheduled))
            .collect();
        let Some((at, previous)) = &self.baseline else {
            self.baseline = Some((world.simulated_time_ns, counts));
            return;
        };
        if world.simulated_time_ns < *at {
            self.baseline = Some((world.simulated_time_ns, counts));
            return;
        }
        let elapsed = world.simulated_time_ns - at;
        if elapsed < TRAFFIC_WINDOW_NS {
            return;
        }
        let seconds = elapsed as f64 / 1e9;
        let mut sent: BTreeMap<String, f64> = BTreeMap::new();
        let mut received: BTreeMap<String, f64> = BTreeMap::new();
        self.links.clear();
        for (key, count) in &counts {
            let before = previous.get(key).copied().unwrap_or(0);
            let rate = count.saturating_sub(before) as f64 / seconds;
            self.links.insert(key.clone(), rate);
            *sent.entry(key.0.clone()).or_default() += rate;
            *received.entry(key.1.clone()).or_default() += rate;
        }
        for machine in &world.machines {
            let history = self.sent.entry(machine.name.clone()).or_default();
            history.push_back(sent.get(&machine.name).copied().unwrap_or(0.0));
            while history.len() > TRAFFIC_SAMPLES {
                history.pop_front();
            }
        }
        self.received = received;
        self.baseline = Some((world.simulated_time_ns, counts));
    }

    pub fn sent_rate(&self, machine: &str) -> f64 {
        self.sent
            .get(machine)
            .and_then(|h| h.back().copied())
            .unwrap_or(0.0)
    }

    pub fn received_rate(&self, machine: &str) -> f64 {
        self.received.get(machine).copied().unwrap_or(0.0)
    }

    pub fn link_rate(&self, from: &str, to: &str) -> f64 {
        self.links
            .get(&(from.to_owned(), to.to_owned()))
            .copied()
            .unwrap_or(0.0)
    }
}

impl Focus {
    fn new(id: SimulationId) -> Self {
        Self {
            id,
            resource: None,
            fault_types: None,
            world: None,
            playback: None,
            frames: VecDeque::new(),
            frame_time_ns: 0,
            events: VecDeque::new(),
            logs: BTreeMap::new(),
            log_sequence: 0,
            event_sequence: 0,
            pending_heals: BTreeMap::new(),
            selected: None,
            checkpoints: Vec::new(),
            resume_rate: PlaybackRate::Unlimited,
            lost: 0,
            timeline: Vec::new(),
            transitions: Vec::new(),
            traffic: Traffic::default(),
            seen: Vec::new(),
            fault_selected: None,
            preparation: BTreeMap::new(),
        }
    }

    /// Fault spans, upcoming inputs and machine downtime as of now.
    pub fn activity(&self) -> Activity {
        crate::activity::derive(
            self.world.as_ref(),
            &self.timeline,
            &self.transitions,
            self.now_ns(Instant::now()),
        )
    }

    /// The handle a heal would close: the selected active fault's, else the
    /// oldest active fault with a handle.
    pub fn healable(&self, activity: &Activity) -> Option<String> {
        let selected = self
            .fault_selected
            .as_ref()
            .and_then(|key| activity.active.iter().find(|item| &item.key == key))
            .and_then(|item| item.handle.clone());
        selected.or_else(|| activity.active.iter().find_map(|item| item.handle.clone()))
    }

    /// Defaults and shortcuts for the inject palette.
    fn form_context(&self) -> crate::fault_form::FormContext {
        let activity = self.activity();
        let handles = self
            .world
            .as_ref()
            .map(|w| {
                w.faults
                    .iter()
                    .filter_map(|f| f.plan.as_ref()?["handle"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        crate::fault_form::FormContext {
            machines: self.machines(),
            selected: self.selected.clone(),
            handles,
            active: activity
                .active
                .iter()
                .filter_map(|item| {
                    Some((
                        item.handle.clone()?,
                        format!("{} {}", item.action, item.target),
                    ))
                })
                .collect(),
        }
    }

    fn record(&mut self, at_ns: u64, machine: &str, incarnation: u32, change: Change) {
        self.transitions.push(Transition {
            at_ns,
            machine: machine.to_owned(),
            incarnation,
            change,
        });
        if self.transitions.len() > MAX_TRANSITIONS {
            self.transitions.remove(0);
        }
    }

    fn observe_heal_receipt(&mut self, receipt: &serde_json::Value) {
        let action = &receipt["request"]["command"]["action"];
        if action["action"] != "heal" {
            return;
        }
        let Some(handle) = action["handle"].as_str() else {
            return;
        };
        let state = &receipt["state"];
        match state["type"].as_str() {
            Some("accepted") => {
                self.pending_heals.entry(handle.to_owned()).or_insert(None);
            }
            Some("acknowledged") if state["error"].is_null() => {
                let id = state["intervention"]["plan"]["id"]
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok());
                self.pending_heals.insert(handle.to_owned(), id);
            }
            _ => {
                self.pending_heals.remove(handle);
            }
        }
        self.reconcile_heals();
    }

    fn reconcile_heals(&mut self) {
        let Some(world) = &self.world else {
            return;
        };
        self.pending_heals.retain(|handle, id| {
            if let Some(fault) = id.and_then(|id| world.faults.iter().find(|f| f.id == id)) {
                return matches!(
                    fault.status,
                    seedbox_api::FaultStatus::Unrequested | seedbox_api::FaultStatus::Requested
                );
            }
            // Until admission is observed, the original active fault keeps the
            // request pending. A stale pre-admission world cannot clear it.
            world.faults.iter().any(|fault| {
                fault.active
                    && fault
                        .plan
                        .as_ref()
                        .and_then(|plan| serde_json::to_value(plan).ok())
                        .is_some_and(|plan| plan["handle"].as_str() == Some(handle.as_str()))
            })
        });
    }

    fn log_gap(&mut self, records: u64, detail: &str) {
        self.lost = self.lost.saturating_add(records);
        let time_ns = self.now_ns(Instant::now());
        for name in self.machines() {
            self.logs.entry(name).or_default();
        }
        for log in self.logs.values_mut() {
            let text = if records == 0 {
                format!("[{detail}]")
            } else {
                format!("[{records} {detail}]")
            };
            log.note(time_ns, text);
        }
    }

    /// Machine names in display order, straight from the observed world.
    pub fn machines(&self) -> Vec<String> {
        self.world
            .as_ref()
            .map(|w| w.machines.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn paused(&self) -> bool {
        matches!(
            self.playback.as_ref().map(|(p, _)| p.state.rate),
            Some(PlaybackRate::Paused)
        )
    }

    /// Whether the simulation is executing, so playback controls apply.
    /// Before the resource is first observed this is assumed, and the server
    /// remains the judge of any control sent.
    pub fn executing(&self) -> bool {
        !self.terminal()
            && self.resource.as_ref().is_none_or(|r| {
                r.status.phase == seedbox_api::Phase::Running
                    && r.status.stage == Some(seedbox_api::Stage::Executing)
            })
    }

    /// Whether this run starts from a checkpoint (a branch, or a rerun of one),
    /// so its machines are restored rather than booted.
    pub fn restoring(&self) -> bool {
        self.resource
            .as_ref()
            .is_some_and(|r| match &r.spec.inputs {
                seedbox_api::InputSource::Resolved { definition } => matches!(
                    definition.start,
                    seedbox_spec::definition::Start::Checkpoint { .. }
                ),
                _ => false,
            })
    }

    /// Why the simulation is not executing yet, if it is starting up.
    pub fn starting(&self) -> Option<seedbox_api::Phase> {
        let resource = self.resource.as_ref()?;
        match resource.status.phase {
            seedbox_api::Phase::Queued | seedbox_api::Phase::Preparing => {
                Some(resource.status.phase)
            }
            seedbox_api::Phase::Running
                if resource.status.stage != Some(seedbox_api::Stage::Executing) =>
            {
                Some(seedbox_api::Phase::Running)
            }
            _ => None,
        }
    }

    pub fn terminal(&self) -> bool {
        self.playback
            .as_ref()
            .is_some_and(|(p, _)| p.state.terminal)
            || self
                .resource
                .as_ref()
                .is_some_and(|r| r.status.phase == seedbox_api::Phase::Completed)
    }

    /// Newest committed simulation time reported by a server resource or feed.
    pub fn now_ns(&self, _wall: Instant) -> u64 {
        let mut now = self.frame_time_ns;
        if let Some(world) = &self.world {
            now = now.max(world.simulated_time_ns);
        }
        if let Some(resource) = &self.resource {
            now = now.max(resource.status.simulated_time_ns);
        }
        if let Some((sample, _)) = &self.playback {
            now = now.max(sample.state.position.simulated_time_ns);
        }
        now
    }

    fn link_latency(&self, from: &str, to: &str) -> u64 {
        self.world
            .as_ref()
            .and_then(|w| w.links.iter().find(|l| l.from == from && l.to == to))
            .map_or(1_000_000, |l| l.policy.latency_ns.max(1))
    }

    /// Drop frames that have arrived, or dropped frames shown long enough.
    pub fn prune_frames(&mut self, now: u64) {
        let lingering: Vec<bool> = self
            .frames
            .iter()
            .map(|f| match (f.fate, f.due_ns) {
                (FrameFate::Scheduled, Some(due)) => due >= now,
                _ => {
                    f.sent_ns
                        .saturating_add(2 * self.link_latency(&f.from, &f.to))
                        >= now
                }
            })
            .collect();
        let mut keep = lingering.into_iter();
        self.frames.retain(|_| keep.next().unwrap_or(false));
        while self.frames.len() > MAX_FRAMES {
            self.frames.pop_front();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FaultMenu {
    Opening,
    Editing(PlaybackRate),
    Finishing,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HitTarget {
    Screen(Screen),
    Catalogue(usize),
    Machine(usize),
    Timeline(usize),
    /// A row of the faults panel or a span on the time bar, by activity row.
    Fault(usize),
    Faults,
    Logs,
}

/// The simulation panel that arrow keys move within.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Panel {
    #[default]
    Machines,
    Faults,
    Logs,
}

pub struct App {
    pub(super) hitboxes: std::cell::RefCell<Vec<(ratatui::layout::Rect, HitTarget)>>,
    pub(super) panel: Panel,
    pub(super) logs_scroll: usize,
    pub(super) events_scroll: usize,
    /// Show raw server events in place of logs.
    pub(super) events_tab: bool,
    /// Keys typed while the inject palette is opening.
    typeahead: Vec<KeyEvent>,
    /// Time bar zoom: each step halves the visible window.
    pub(super) zoom: u32,
    /// Whether the newest catalogue poll reached the server.
    pub connected: bool,
    /// Observed faults and timelines of catalogue rows, for the detail pane.
    pub previews: BTreeMap<SimulationId, Preview>,
    preview_asked: BTreeMap<SimulationId, Instant>,
    pub screen: Screen,
    pub list: Vec<SimulationSummary>,
    pub list_selected: usize,
    pub focus: Option<Focus>,
    pub notice: Option<Notice>,
    pub inspector: Option<Inspector>,
    pub fault_form: Option<FaultForm>,
    pub fault_menu: Option<FaultMenu>,
    pub timelines: Vec<crate::timeline::TimelineDraft>,
    pub timeline_source: usize,
    pub timeline_row: usize,
    pub timeline_directory: Option<std::path::PathBuf>,
    timeline_confirm: Option<char>,
    pub timeline_name: Option<String>,
    control_pending: bool,
    quit_pending: bool,
    pub comparison: Option<SimulationComparison>,
    pub baseline: Option<SimulationId>,
    pub confirm_delete: Option<SimulationId>,
    /// A rerun waiting for its confirming second `r`.
    confirm_rerun: Option<SimulationId>,
    pub details: bool,
    pub help: bool,
    pub quit: bool,
}

pub const RATES: [(u32, u32); 7] = [
    (1, 1000),
    (1, 200),
    (1, 50),
    (1, 10),
    (1, 2),
    (1, 1),
    (4, 1),
];

fn limited(n: u32, d: u32) -> PlaybackRate {
    PlaybackRate::Limited {
        numerator: NonZeroU32::new(n).expect("nonzero"),
        denominator: NonZeroU32::new(d).expect("nonzero"),
    }
}

/// Position on the rate ladder: 0..RATES.len() for limited rates, one past
/// the end for unlimited.
fn rate_index(rate: PlaybackRate) -> usize {
    match rate {
        PlaybackRate::Limited {
            numerator,
            denominator,
        } => {
            let value = f64::from(numerator.get()) / f64::from(denominator.get());
            RATES
                .iter()
                .position(|(n, d)| f64::from(*n) / f64::from(*d) >= value)
                .unwrap_or(RATES.len() - 1)
        }
        _ => RATES.len(),
    }
}

fn ladder(index: usize) -> PlaybackRate {
    RATES
        .get(index)
        .map_or(PlaybackRate::Unlimited, |(n, d)| limited(*n, *d))
}

impl App {
    pub fn with_timelines(mut self, directory: std::path::PathBuf) -> std::io::Result<Self> {
        let (timelines, skipped) = crate::timeline::TimelineDraft::load_all(&directory)?;
        self.timelines = timelines;
        self.timeline_directory = Some(directory);
        if let Some(first) = skipped.first() {
            let more = skipped.len() - 1;
            self.notify(
                if more == 0 {
                    format!("saved timeline skipped: {first}")
                } else {
                    format!("saved timeline skipped: {first} (and {more} more)")
                },
                Tone::Bad,
            );
        }
        Ok(self)
    }

    fn timeline_update(&mut self, timeline: seedbox_api::FaultTimeline) {
        let Some(resource) = self
            .focus
            .as_ref()
            .and_then(|focus| focus.resource.as_ref())
            .filter(|resource| resource.metadata.id == timeline.simulation)
        else {
            return;
        };
        let created = resource.metadata.created_at_unix_ms;
        let index = self
            .timelines
            .iter()
            .position(|draft| {
                draft.source() == &timeline.simulation && draft.created_at_unix_ms() == created
            })
            .unwrap_or_else(|| {
                self.timelines.push(crate::timeline::TimelineDraft::new(
                    timeline.simulation.clone(),
                    created,
                ));
                self.timelines.len() - 1
            });
        let application = simulation_name(resource).to_owned();
        let mut metadata = self.timelines[index].metadata().clone();
        if metadata.name == timeline.simulation.as_str() {
            metadata.name = application.clone();
        }
        metadata.application = application;
        metadata.seed = resource.spec.seed;
        metadata.recorded_at_unix_ms = created;
        let draft = &mut self.timelines[index];
        draft.attach_definition(resource);
        draft.set_metadata(metadata);
        let result = draft.merge(&timeline.entries);
        if let Err(error) = result {
            self.notify(format!("timeline update: {error}"), Tone::Bad);
        } else if draft.is_unsaved() {
            if let Some(directory) = &self.timeline_directory {
                if let Err(error) = draft.retry_save(directory) {
                    self.notify(format!("timeline save failed: {error}"), Tone::Bad);
                }
            }
        }
    }

    fn timeline_matches_focus(&self) -> bool {
        self.timelines
            .get(self.timeline_source)
            .is_some_and(|draft| {
                self.focus
                    .as_ref()
                    .and_then(|f| f.resource.as_ref())
                    .is_some_and(|r| {
                        &r.metadata.id == draft.source()
                            && r.metadata.created_at_unix_ms == draft.created_at_unix_ms()
                    })
            })
    }

    fn timeline_key(&mut self, key: KeyEvent) -> Vec<Request> {
        if key.code == KeyCode::Enter {
            let Some(draft) = self.timelines.get(self.timeline_source) else {
                return Vec::new();
            };
            if let Some(mut document) = draft.submission_snapshot() {
                let how = if draft.modified_count() > 0 {
                    "edited"
                } else {
                    "rerun"
                };
                crate::api::annotate(&mut document, draft.source(), how);
                self.notify(
                    format!("submitting {how} timeline of {}…", draft.source()),
                    Tone::Info,
                );
                return vec![Request::SubmitDefinition(document)];
            }
            self.notify(
                "complete prepared definition unavailable; open the simulation after preparation",
                Tone::Info,
            );
            return Vec::new();
        }
        if key.code == KeyCode::Char('p') {
            if !self.timeline_matches_focus() {
                self.notify(
                    "open this timeline's source before controlling playback",
                    Tone::Info,
                );
                return Vec::new();
            }
            let Some(focus) = self.focus.as_ref().filter(|focus| !focus.terminal()) else {
                self.notify("source simulation has completed", Tone::Info);
                return Vec::new();
            };
            let rate = match self.fault_menu {
                Some(FaultMenu::Editing(rate)) if rate != PlaybackRate::Paused => rate,
                _ => focus.resume_rate,
            };
            let id = focus.id.clone();
            self.fault_menu = None;
            return vec![Request::Control(
                id,
                ControlCommand::PauseBeforeNextFault { rate },
            )];
        }
        if key.code == KeyCode::Char('N') {
            if !self.timeline_matches_focus() {
                self.notify(
                    "open this timeline's source to locate its next fault",
                    Tone::Info,
                );
                return Vec::new();
            }
            let next = self.timelines.get(self.timeline_source).and_then(|d| {
                self.focus
                    .as_ref()
                    .and_then(|f| f.world.as_ref())
                    .and_then(|world| d.next_upcoming(world))
            });
            if let Some(index) = next {
                self.timeline_row = index;
            } else {
                self.notify("no upcoming faults in this simulation", Tone::Info);
            }
            return Vec::new();
        }
        if key.code == KeyCode::Char('n') {
            self.timeline_name = self
                .timelines
                .get(self.timeline_source)
                .map(|d| d.metadata().name.clone());
            return Vec::new();
        }
        if key.code == KeyCode::Char('w') {
            if let (Some(draft), Some(directory)) = (
                self.timelines.get_mut(self.timeline_source),
                &self.timeline_directory,
            ) {
                let result = draft.save_definition(directory);
                self.notify(
                    match result {
                        Ok(path) => format!("simulation definition saved: {}", path.display()),
                        Err(e) => format!("timeline not saved: {e}"),
                    },
                    Tone::Info,
                );
            }
            return Vec::new();
        }
        if key.code == KeyCode::Char(' ') {
            if !self.timeline_matches_focus() {
                self.notify("this saved timeline is not the focused simulation; open its source before controlling playback", Tone::Info);
                return Vec::new();
            }
            return self.simulation_key(key);
        }

        if key.code == KeyCode::Esc {
            self.screen = if self.focus.is_some() {
                Screen::Simulation
            } else {
                Screen::Catalogue
            };
            return Vec::new();
        }
        if matches!(key.code, KeyCode::Char('[' | ']')) && !self.timelines.is_empty() {
            let n = self.timelines.len();
            self.timeline_source = (self.timeline_source
                + if key.code == KeyCode::Char(']') {
                    1
                } else {
                    n - 1
                })
                % n;
            self.timeline_row = 0;
            self.timeline_confirm = None;
            return Vec::new();
        }
        let Some(draft) = self.timelines.get_mut(self.timeline_source) else {
            return Vec::new();
        };
        let mut changed = false;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.timeline_row = self.timeline_row.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.timeline_row =
                    (self.timeline_row + 1).min(draft.entries().len().saturating_sub(1))
            }
            KeyCode::Char('d') | KeyCode::Delete => changed = draft.toggle(self.timeline_row),
            KeyCode::Char('u') => changed = draft.undo(),
            KeyCode::Char(c @ ('C' | 'R')) => {
                if self.timeline_confirm == Some(c) {
                    changed = if c == 'C' {
                        draft.clear()
                    } else {
                        draft.reset()
                    };
                } else {
                    self.timeline_confirm = Some(c);
                    self.notify(
                        format!(
                            "press {c} again to {} the edited timeline",
                            if c == 'C' { "clear" } else { "reset" }
                        ),
                        Tone::Info,
                    );
                    return Vec::new();
                }
            }
            _ => {}
        }
        self.timeline_confirm = None;
        if changed {
            if let Some(directory) = &self.timeline_directory {
                match draft.save(directory) {
                    Ok(()) => self.notify(
                        "working copy stored; w saves a simulation definition",
                        Tone::Good,
                    ),
                    Err(error) => self.notify(format!("timeline not saved: {error}"), Tone::Bad),
                }
            }
        }
        Vec::new()
    }

    pub fn new(focus: Option<SimulationId>) -> Self {
        Self {
            hitboxes: Default::default(),
            panel: Panel::Machines,
            logs_scroll: 0,
            events_scroll: 0,
            events_tab: false,
            typeahead: Vec::new(),
            zoom: 0,
            connected: true,
            previews: BTreeMap::new(),
            preview_asked: BTreeMap::new(),
            screen: if focus.is_some() {
                Screen::Simulation
            } else {
                Screen::Catalogue
            },
            list: Vec::new(),
            list_selected: 0,
            focus: focus.map(Focus::new),
            notice: None,
            inspector: None,
            fault_form: None,
            fault_menu: None,
            timelines: Vec::new(),
            timeline_source: 0,
            timeline_row: 0,
            timeline_directory: None,
            timeline_confirm: None,
            timeline_name: None,
            control_pending: false,
            quit_pending: false,
            comparison: None,
            baseline: None,
            confirm_delete: None,
            confirm_rerun: None,
            details: false,
            help: false,
            quit: false,
        }
    }

    fn notify(&mut self, text: impl Into<String>, tone: Tone) {
        self.notice = Some(Notice {
            text: text.into(),
            tone,
            at: Instant::now(),
        });
    }

    pub fn notice(&self, now: Instant) -> Option<&Notice> {
        self.notice
            .as_ref()
            .filter(|n| now.saturating_duration_since(n.at) < NOTICE_FOR)
    }

    pub fn inspector(&self, now: Instant) -> Option<&Inspector> {
        self.inspector
            .as_ref()
            .filter(|i| now.saturating_duration_since(i.at) < NOTICE_FOR)
    }

    /// Catalogue rows in tree order: each simulation followed by the runs
    /// derived from it (reruns, edited timelines and branches).
    pub fn tree(&self) -> Vec<(usize, &SimulationSummary)> {
        let ids: std::collections::BTreeSet<&SimulationId> =
            self.list.iter().map(|s| &s.metadata.id).collect();
        let mut out = Vec::new();
        let mut placed = std::collections::BTreeSet::new();
        fn visit<'a>(
            list: &'a [SimulationSummary],
            parent: Option<&SimulationId>,
            depth: usize,
            placed: &mut std::collections::BTreeSet<SimulationId>,
            out: &mut Vec<(usize, &'a SimulationSummary)>,
        ) {
            let mut children: Vec<&'a SimulationSummary> = list
                .iter()
                .filter(|s| source_of(s).as_ref() == parent)
                .collect();
            // Top-level runs group by project, newest first within each.
            if parent.is_none() {
                children.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
            }
            for s in children {
                if !placed.insert(s.metadata.id.clone()) {
                    continue;
                }
                out.push((depth, s));
                visit(list, Some(&s.metadata.id), depth + 1, placed, out);
            }
        }
        visit(&self.list, None, 0, &mut placed, &mut out);
        // Runs whose source is not listed (deleted, or on a later page).
        for s in &self.list {
            if source_of(s).is_some_and(|p| !ids.contains(&p)) && !placed.contains(&s.metadata.id) {
                placed.insert(s.metadata.id.clone());
                out.push((0, s));
                visit(&self.list, Some(&s.metadata.id), 1, &mut placed, &mut out);
            }
        }
        // Lineage cycles have no root; list their members rather than lose them.
        for s in &self.list {
            if placed.insert(s.metadata.id.clone()) {
                out.push((0, s));
                visit(&self.list, Some(&s.metadata.id), 1, &mut placed, &mut out);
            }
        }
        out
    }

    /// Fetch the selected catalogue row and its source for the detail pane,
    /// at most every couple of seconds; completed runs are fetched once.
    fn preview_requests(&mut self) -> Vec<Request> {
        if self.screen != Screen::Catalogue || !self.connected {
            return Vec::new();
        }
        let Some(summary) = self
            .tree()
            .get(self.list_selected)
            .map(|(_, s)| (*s).clone())
        else {
            return Vec::new();
        };
        let mut ids = vec![summary.metadata.id.clone()];
        ids.extend(source_of(&summary));
        let mut out = Vec::new();
        for id in ids {
            let completed = self
                .list
                .iter()
                .any(|s| s.metadata.id == id && s.status.phase == seedbox_api::Phase::Completed);
            let fresh = self
                .previews
                .get(&id)
                .is_some_and(|p| completed || p.fetched.elapsed() < PREVIEW_EVERY);
            let asked = self
                .preview_asked
                .get(&id)
                .is_some_and(|at| at.elapsed() < PREVIEW_EVERY);
            if !fresh && !asked {
                self.preview_asked.insert(id.clone(), Instant::now());
                out.push(Request::Preview(id));
            }
        }
        out
    }

    fn selected_summary(&self) -> Option<SimulationId> {
        self.tree()
            .get(self.list_selected)
            .map(|(_, s)| s.metadata.id.clone())
    }

    fn close_terminal_menu(&mut self) {
        if self.focus.as_ref().is_some_and(Focus::terminal) {
            self.fault_form = None;
            self.fault_menu = None;
            self.control_pending = false;
            if self.quit_pending {
                self.quit = true;
            }
        }
    }

    pub fn apply(&mut self, update: Update) -> Vec<Request> {
        let width = self
            .hitboxes
            .borrow()
            .iter()
            .find(|(_, target)| *target == HitTarget::Logs)
            .map_or(80, |(rect, _)| rect.width.saturating_sub(2));
        let before = if self.logs_scroll > 0 {
            self.focus
                .as_ref()
                .map_or(0, |focus| crate::view::log_rows(focus, width).len())
        } else {
            0
        };
        let mut requests = self.apply_update(update);
        requests.extend(self.preview_requests());
        if self.logs_scroll > 0 {
            let after = self
                .focus
                .as_ref()
                .map_or(0, |focus| crate::view::log_rows(focus, width).len());
            self.logs_scroll = self
                .logs_scroll
                .saturating_add(after.saturating_sub(before));
        }
        requests
    }

    fn apply_update(&mut self, update: Update) -> Vec<Request> {
        let mut requests = Vec::new();
        if self.focus.as_ref().is_some_and(Focus::terminal)
            && matches!(
                update,
                Update::FaultMenuReady(_) | Update::FaultRejected(_, _) | Update::FaultMenuDone(_)
            )
        {
            self.close_terminal_menu();
            return requests;
        }
        match update {
            Update::Focused(id, update) => {
                if self.focus_for(&id).is_some() {
                    return self.apply_update(*update);
                }
            }
            Update::Missing(id) => {
                if self.focus_for(&id).is_some() {
                    self.focus = None;
                    self.fault_form = None;
                    self.fault_menu = None;
                    if self.quit_pending {
                        self.quit = true;
                    }
                    self.screen = Screen::Catalogue;
                    self.inspector = None;
                    self.notify(
                        format!("{id} is no longer available on the server"),
                        Tone::Bad,
                    );
                    requests.push(Request::Close);
                }
            }
            Update::List(mut list) => {
                let selected = self.selected_summary();
                list.sort_by(|a, b| {
                    b.metadata
                        .created_at_unix_ms
                        .cmp(&a.metadata.created_at_unix_ms)
                        .then_with(|| a.metadata.id.cmp(&b.metadata.id))
                });
                self.list = list;
                self.list_selected = selected
                    .and_then(|id| self.tree().iter().position(|(_, s)| s.metadata.id == id))
                    .unwrap_or_else(|| self.list_selected.min(self.list.len().saturating_sub(1)));
            }
            Update::Resource(resource) => {
                if let Some(focus) = self.focus_for(&resource.metadata.id) {
                    if focus.resource.as_ref().is_none_or(|current| {
                        current.metadata.revision <= resource.metadata.revision
                    }) {
                        focus.resource = Some(resource);
                    }
                }
                self.close_terminal_menu();
            }
            Update::World(world) => {
                if let Some(focus) = self.focus_for(&world.simulation) {
                    if focus.world.as_ref().is_some_and(|current| {
                        current.revision > world.revision
                            || (current.revision == world.revision
                                && current.simulated_time_ns > world.simulated_time_ns)
                    }) {
                        return requests;
                    }
                    let names: Vec<&String> = world.machines.iter().map(|m| &m.name).collect();
                    if focus.selected.as_ref().is_some_and(|s| !names.contains(&s)) {
                        focus.selected = None;
                    }
                    for machine in &world.machines {
                        if !focus.seen.contains(&machine.name) {
                            focus.seen.push(machine.name.clone());
                        }
                    }
                    focus.traffic.observe(&world);
                    focus.world = Some(world);
                    focus.reconcile_heals();
                }
            }
            Update::FaultMenuReady(result) => match result {
                Ok(rate) => {
                    self.fault_menu = Some(FaultMenu::Editing(rate));
                    if self.quit_pending {
                        return self.cancel_fault_menu();
                    }
                    if let Some(catalogue) = self.focus.as_ref().and_then(|f| f.fault_types.clone())
                    {
                        let context = self
                            .focus
                            .as_ref()
                            .map(Focus::form_context)
                            .unwrap_or_default();
                        self.fault_form = Some(FaultForm::new(catalogue).with_context(context));
                        for key in std::mem::take(&mut self.typeahead) {
                            requests.extend(self.input_raw(Event::Key(key)));
                        }
                        if requests.iter().any(|r| matches!(r, Request::Fault(..))) {
                            self.fault_menu = Some(FaultMenu::Finishing);
                        }
                    } else {
                        return self.cancel_fault_menu();
                    }
                }
                Err(error) => {
                    self.typeahead.clear();
                    self.fault_menu = Some(FaultMenu::Failed);
                    self.notify(error, Tone::Bad);
                    if self.quit_pending {
                        self.quit = true;
                    }
                }
            },
            Update::FaultRejected(rate, error) => {
                self.fault_menu = Some(FaultMenu::Editing(rate));
                if let Some(form) = self.fault_form.as_mut() {
                    form.set_error(error);
                }
                if self.quit_pending {
                    return self.cancel_fault_menu();
                }
            }
            Update::FaultMenuDone(ok) => {
                if ok {
                    self.fault_form = None;
                }
                self.fault_menu = if ok { None } else { Some(FaultMenu::Failed) };
                if self.quit_pending {
                    self.quit = true;
                }
            }
            Update::ControlDone => {
                self.control_pending = false;
                if self.quit_pending {
                    self.quit = true;
                }
            }
            Update::Playback(sample) => {
                if let Some(focus) = self.focus.as_mut() {
                    if !matches!(sample.state.rate, PlaybackRate::Paused) {
                        focus.resume_rate = sample.state.rate;
                    }
                    focus.playback = Some((sample, Instant::now()));
                }
                self.close_terminal_menu();
            }
            Update::Event(delivery) => {
                if let Some(focus) = self.focus.as_mut() {
                    match delivery {
                        EventDelivery::Event {
                            sequence,
                            observation,
                        } if sequence > focus.event_sequence => {
                            focus.event_sequence = sequence;
                            let at = observation
                                .simulated_time_ns
                                .unwrap_or_else(|| focus.now_ns(Instant::now()));
                            match &observation.event {
                                EventKind::MachineLifecycle {
                                    machine,
                                    incarnation,
                                    transition,
                                } => focus.record(
                                    at,
                                    machine,
                                    *incarnation,
                                    Change::from_lifecycle(transition),
                                ),
                                EventKind::MachineReady {
                                    machine,
                                    incarnation,
                                } => focus.record(at, machine, *incarnation, Change::Ready),
                                EventKind::MachineExited {
                                    machine,
                                    incarnation,
                                    code,
                                } => focus.record(at, machine, *incarnation, Change::Exited(*code)),
                                EventKind::PreparationProgress { machine, stage } => {
                                    focus.preparation.insert(machine.clone(), *stage);
                                }
                                _ => {}
                            }
                            if let EventKind::ControlChanged { control } = &observation.event {
                                focus.observe_heal_receipt(control);
                            }
                            let refresh = matches!(
                                &observation.event,
                                EventKind::FaultChanged { .. }
                                    | EventKind::OverrideRejected { .. }
                                    | EventKind::MachineLifecycle { .. }
                                    | EventKind::MachineReady { .. }
                                    | EventKind::MachineExited { .. }
                                    | EventKind::ControlChanged { .. }
                            );
                            if refresh {
                                requests.push(Request::RefreshWorld(focus.id.clone()));
                            }
                            focus.events.push_back(observation);
                            if focus.events.len() > MAX_EVENTS {
                                focus.events.pop_front();
                            }
                        }
                        EventDelivery::Event { .. } => {}
                        EventDelivery::Gap { first, last } if last > focus.event_sequence => {
                            let first = first.max(focus.event_sequence + 1);
                            focus.lost += last - first + 1;
                            focus.event_sequence = last;
                        }
                        EventDelivery::Gap { .. } => {}
                        EventDelivery::Closed => {}
                    }
                }
            }
            Update::Log(id, delivery) => {
                if let Some(focus) = self.focus_for(&id) {
                    let entry = match delivery {
                        LogDelivery::Entry { sequence, entry } => {
                            if sequence <= focus.log_sequence {
                                return requests;
                            }
                            focus.log_sequence = sequence;
                            Some(entry)
                        }
                        LogDelivery::Gap { first, last } => {
                            let first = first.max(focus.log_sequence.saturating_add(1));
                            if first <= last {
                                focus.log_sequence = last;
                                focus.log_gap(last - first + 1, "output records were lost");
                            }
                            None
                        }
                        LogDelivery::Closed => None,
                    };
                    match entry {
                        Some(LogEntry::Bytes {
                            machine,
                            simulated_time_ns,
                            stream,
                            bytes,
                            ..
                        }) => {
                            focus.logs.entry(machine).or_default().push(
                                simulated_time_ns,
                                stream,
                                &bytes,
                            );
                        }
                        Some(LogEntry::MachineGap {
                            machine,
                            simulated_time_ns,
                            records,
                            ..
                        }) => focus.logs.entry(machine).or_default().note(
                            simulated_time_ns,
                            format!("[{records} output records were lost]"),
                        ),
                        Some(LogEntry::SourceGap { first, last }) => {
                            focus.log_gap(last - first + 1, "worker output records were lost");
                        }
                        Some(LogEntry::SourceUnavailable) => {
                            focus.log_gap(0, "output source unavailable");
                        }
                        None => {}
                    }
                }
            }
            Update::Frames(delivery) => {
                if let Some(focus) = self.focus.as_mut() {
                    match delivery {
                        FrameDelivery::Frames {
                            simulated_time_ns,
                            frames,
                            ..
                        } => {
                            focus.frame_time_ns = focus.frame_time_ns.max(simulated_time_ns);
                            focus.frames.extend(frames);
                            let now = focus.now_ns(Instant::now());
                            focus.prune_frames(now);
                        }
                        FrameDelivery::Gap { first, last } => focus.lost += last - first + 1,
                        FrameDelivery::Closed => {}
                    }
                }
            }
            Update::Receipt {
                title,
                request,
                result,
            } => {
                if title == "checkpoint"
                    && result
                        .as_ref()
                        .is_ok_and(|r| r.state == seedbox_api::ControlState::Accepted)
                {
                    self.notify(
                        "checkpoint requested; it is captured at the next boundary (step or resume)",
                        Tone::Info,
                    );
                }
                let (ok, result) = match result {
                    Ok(receipt) => {
                        let ok = match &receipt.state {
                            seedbox_api::ControlState::Acknowledged { error, .. } => {
                                error.is_none()
                            }
                            seedbox_api::ControlState::Accepted => true,
                            _ => false,
                        };
                        (ok, describe_receipt(&receipt))
                    }
                    Err(error) => (false, error),
                };
                self.inspector = Some(Inspector {
                    title,
                    request,
                    result,
                    ok,
                    at: Instant::now(),
                });
                if let Some(focus) = &self.focus {
                    requests.push(Request::RefreshWorld(focus.id.clone()));
                }
            }
            Update::Timeline(timeline) => {
                if let Some(focus) = self.focus_for(&timeline.simulation) {
                    focus.timeline.clone_from(&timeline.entries);
                }
                self.timeline_update(timeline)
            }
            Update::Connection(connected) => {
                if connected != self.connected {
                    self.connected = connected;
                    if connected {
                        self.notify("reconnected to the local server", Tone::Good);
                    }
                }
            }
            Update::FaultTypes(catalogue) => {
                if self
                    .focus
                    .as_ref()
                    .is_some_and(|focus| focus.id == catalogue.simulation)
                {
                    if let Some(form) = self.fault_form.as_mut() {
                        form.update_catalogue(catalogue.clone());
                    }
                    if let Some(focus) = self.focus.as_mut() {
                        focus.fault_types = Some(catalogue);
                    }
                }
            }
            Update::FaultReceipt {
                title,
                request,
                result,
            } => {
                if let (Some(focus), Ok(receipt)) = (&self.focus, &result) {
                    if let Some(entry) = receipt_timeline_entry(receipt) {
                        let timeline = seedbox_api::FaultTimeline {
                            simulation: focus.id.clone(),
                            entries: vec![entry],
                        };
                        self.timeline_update(timeline);
                    }
                }

                let action = serde_json::from_str::<serde_json::Value>(&request)
                    .ok()
                    .and_then(|request| {
                        request
                            .get("command")
                            .and_then(|c| c.get("action"))
                            .cloned()
                    });
                let (ok, text) = match &result {
                    Ok(receipt) => {
                        let state = &receipt["state"];
                        let ok = state["type"] == "accepted"
                            || (state["type"] == "acknowledged" && state["error"].is_null());
                        if let Some(handle) = action
                            .as_ref()
                            .filter(|a| a["action"] == "heal")
                            .and_then(|a| a["handle"].as_str())
                        {
                            if let Some(focus) = self.focus.as_mut() {
                                if ok {
                                    let id = state["intervention"]["plan"]["id"]
                                        .as_u64()
                                        .and_then(|id| u32::try_from(id).ok());
                                    focus.pending_heals.insert(handle.to_owned(), id);
                                } else {
                                    focus.pending_heals.remove(handle);
                                }
                            }
                        }
                        (
                            ok,
                            format!(
                                "{}{}{}",
                                state["type"].as_str().unwrap_or("unknown"),
                                state["error"]
                                    .as_str()
                                    .map(|error| format!(": {error}"))
                                    .unwrap_or_default(),
                                state["detail"]
                                    .as_str()
                                    .map(|detail| format!(" — {detail}"))
                                    .unwrap_or_default()
                            ),
                        )
                    }
                    Err(error) => {
                        if let Some(handle) = action
                            .as_ref()
                            .filter(|a| a["action"] == "heal")
                            .and_then(|a| a["handle"].as_str())
                        {
                            if let Some(focus) = self.focus.as_mut() {
                                focus.pending_heals.remove(handle);
                            }
                        }
                        (false, error.clone())
                    }
                };
                self.inspector = Some(Inspector {
                    title,
                    request,
                    result: text,
                    ok,
                    at: Instant::now(),
                });
                if let Some(focus) = &self.focus {
                    requests.push(Request::RefreshWorld(focus.id.clone()));
                }
            }
            Update::Checkpoints(checkpoints) => {
                if let Some(focus) = self.focus.as_mut() {
                    focus.checkpoints = checkpoints;
                }
            }
            Update::Comparison(comparison) => self.comparison = Some(comparison),
            Update::Preview(id, world, timeline) => {
                self.previews.insert(
                    id,
                    Preview {
                        world: world.map(|w| *w),
                        timeline,
                        fetched: Instant::now(),
                    },
                );
            }
            Update::Submitted {
                title,
                request,
                result,
            } => {
                let (ok, text) = match &result {
                    Ok(simulation) => (true, format!("created {}", simulation.metadata.id)),
                    Err(error) => (false, error.clone()),
                };
                // Follow a new rerun or branch into its own view.
                let opened = match &result {
                    Ok(simulation)
                        if matches!(self.screen, Screen::Simulation | Screen::Timeline) =>
                    {
                        self.open(simulation.metadata.id.clone())
                    }
                    _ => Vec::new(),
                };
                requests.extend(opened);
                self.notify(
                    format!("{title}: {text}"),
                    if ok { Tone::Good } else { Tone::Bad },
                );
                self.inspector = Some(Inspector {
                    title,
                    request,
                    result: text,
                    ok,
                    at: Instant::now(),
                });
            }
            Update::Notice(text) => self.notify(text, Tone::Info),
            Update::Error(text) => self.notify(text, Tone::Bad),
        }
        requests
    }

    fn focus_for(&mut self, id: &SimulationId) -> Option<&mut Focus> {
        self.focus.as_mut().filter(|f| &f.id == id)
    }

    fn cancel_fault_menu(&mut self) -> Vec<Request> {
        self.fault_form = None;
        if let Some(FaultMenu::Editing(rate)) = self.fault_menu {
            self.fault_menu = Some(FaultMenu::Finishing);
            return self
                .focus
                .as_ref()
                .map(|f| Request::EndFaultMenu(f.id.clone(), rate))
                .into_iter()
                .collect();
        }
        Vec::new()
    }

    fn request_quit(&mut self) -> Vec<Request> {
        self.quit_pending = true;
        if matches!(self.fault_menu, Some(FaultMenu::Editing(_))) {
            return self.cancel_fault_menu();
        }
        if !self.control_pending
            && !matches!(
                self.fault_menu,
                Some(FaultMenu::Opening | FaultMenu::Finishing)
            )
        {
            self.quit = true;
        }
        Vec::new()
    }

    pub fn input(&mut self, event: Event) -> Vec<Request> {
        let mut requests = self.input_raw(event);
        requests.extend(self.preview_requests());
        let mut blocked = false;
        let mut requests: Vec<_> = requests
            .into_iter()
            .filter(|request| {
                let (Request::Fault(id, action, _) | Request::Heal(id, action)) = request else {
                    return true;
                };
                if action["action"] != "heal" {
                    return true;
                }
                let Some(handle) = action["handle"].as_str() else {
                    return true;
                };
                let Some(focus) = self.focus_for(id) else {
                    return true;
                };
                let observed_pending = focus.world.as_ref().is_some_and(|world| {
                    world.faults.iter().any(|fault| {
                        matches!(
                            fault.status,
                            seedbox_api::FaultStatus::Unrequested
                                | seedbox_api::FaultStatus::Requested
                        ) && fault.plan.as_ref().is_some_and(|plan| {
                            plan["action"] == "heal" && plan["handle"].as_str() == Some(handle)
                        })
                    })
                });
                if observed_pending || focus.pending_heals.contains_key(handle) {
                    blocked = true;
                    return false;
                }
                focus.pending_heals.insert(handle.to_owned(), None);
                true
            })
            .collect();
        if blocked {
            let text = if self.focus.as_ref().is_some_and(Focus::paused) {
                "heal already queued; resume to apply"
            } else {
                "heal already pending"
            };
            self.notify(text, Tone::Info);
            // A duplicate heal is a cancellation, so restore the menu's rate too.
            if let Some(FaultMenu::Editing(rate)) = self.fault_menu {
                self.fault_menu = Some(FaultMenu::Finishing);
                if let Some(focus) = &self.focus {
                    requests.push(Request::EndFaultMenu(focus.id.clone(), rate));
                }
            }
        } else if requests.iter().any(|r| matches!(r, Request::Fault(..))) {
            self.fault_menu = Some(FaultMenu::Finishing);
        }
        if requests
            .iter()
            .any(|r| matches!(r, Request::Control(..) | Request::PauseThenStep(..)))
        {
            self.control_pending = true;
        }
        requests
    }

    fn input_raw(&mut self, event: Event) -> Vec<Request> {
        if let Event::Mouse(mouse) = event {
            return self.mouse(mouse);
        }
        let Event::Key(key) = event else {
            return Vec::new();
        };
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if key.code != KeyCode::Char('r') {
            self.confirm_rerun = None;
        }
        if let Some(name) = self.timeline_name.as_mut() {
            match key.code {
                KeyCode::Esc => self.timeline_name = None,
                KeyCode::Enter => {
                    let name = self.timeline_name.take().unwrap_or_default();
                    if !name.trim().is_empty() {
                        if let Some(draft) = self.timelines.get_mut(self.timeline_source) {
                            draft.rename(name.trim().to_owned());
                            if let Some(directory) = &self.timeline_directory {
                                if let Err(error) = draft.save(directory) {
                                    self.notify(format!("timeline not saved: {error}"), Tone::Bad);
                                }
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => name.clear(),
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL) && name.len() < 120 =>
                {
                    name.push(c)
                }
                _ => {}
            }
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.request_quit();
        }
        // Keys typed while the palette waits for its pause belong to the
        // palette; replay them once it opens rather than dropping them.
        if self.fault_menu == Some(FaultMenu::Opening) && !self.quit_pending {
            if self.typeahead.len() < 64 {
                self.typeahead.push(key);
            }
            return Vec::new();
        }
        if self.fault_form.is_none() && key.code == KeyCode::Char('q') {
            return self.request_quit();
        }
        // Keep focus stable until the outstanding control has reported its result.
        if self.control_pending && !matches!(key.code, KeyCode::Char('1' | '2' | '3')) {
            self.notify("waiting for playback control acknowledgement", Tone::Info);
            return Vec::new();
        }
        if matches!(
            self.fault_menu,
            Some(FaultMenu::Opening | FaultMenu::Finishing)
        ) || self.quit_pending
        {
            return Vec::new();
        }
        if self.fault_menu == Some(FaultMenu::Failed) {
            if key.code == KeyCode::Esc {
                self.fault_menu = None;
                self.fault_form = None;
            } else if key.code == KeyCode::Char('q') {
                return self.request_quit();
            }
            return Vec::new();
        }
        if let Some(form) = self.fault_form.as_mut() {
            match form.input(key) {
                FormOutcome::None => return Vec::new(),
                FormOutcome::Cancel => {
                    return self.cancel_fault_menu();
                }
                FormOutcome::Submit(action) => {
                    return self
                        .focus
                        .as_ref()
                        .and_then(|f| match self.fault_menu {
                            Some(FaultMenu::Editing(rate)) => {
                                Some(Request::Fault(f.id.clone(), action, rate))
                            }
                            _ => None,
                        })
                        .into_iter()
                        .collect();
                }
            }
        }
        if key.code == KeyCode::Char('v') {
            self.details = !self.details;
            return Vec::new();
        }
        if self.help {
            self.help = false;
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('q') => {
                return self.request_quit();
            }
            KeyCode::Char('?') => {
                self.help = true;
                return Vec::new();
            }
            KeyCode::Char('1') => {
                self.screen = Screen::Catalogue;
                return Vec::new();
            }
            KeyCode::Char('3') => {
                if let Some(resource) = self.focus.as_ref().and_then(|f| f.resource.as_ref()) {
                    if let Some(index) = self.timelines.iter().position(|d| {
                        d.source() == &resource.metadata.id
                            && d.created_at_unix_ms() == resource.metadata.created_at_unix_ms
                    }) {
                        self.timeline_source = index;
                    }
                }
                self.timeline_row = 0;
                self.screen = Screen::Timeline;
                return Vec::new();
            }
            KeyCode::Char('2') if self.focus.is_some() => {
                self.screen = Screen::Simulation;
                return Vec::new();
            }
            _ => {}
        }
        if self.screen == Screen::Simulation
            && matches!(
                key.code,
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
            )
        {
            let offset = if self.events_tab {
                &mut self.events_scroll
            } else {
                &mut self.logs_scroll
            };
            match key.code {
                KeyCode::PageUp => *offset = offset.saturating_add(10),
                KeyCode::PageDown => *offset = offset.saturating_sub(10),
                KeyCode::Home => *offset = usize::MAX,
                KeyCode::End => *offset = 0,
                _ => return self.simulation_key(key),
            }
            return Vec::new();
        }
        match self.screen {
            Screen::Catalogue => self.catalogue_key(key),
            Screen::Simulation => self.simulation_key(key),
            Screen::Timeline => self.timeline_key(key),
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) -> Vec<Request> {
        if self.help || self.timeline_name.is_some() {
            return Vec::new();
        }
        if let Some(form) = self.fault_form.as_mut() {
            if let Some(key) = form.mouse(mouse) {
                return self.input_raw(Event::Key(key));
            }
            return Vec::new();
        }
        if self.fault_menu.is_some() {
            return Vec::new();
        }
        let target = self
            .hitboxes
            .borrow()
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
            .map(|(_, target)| *target);
        let click = mouse.kind == MouseEventKind::Down(MouseButton::Left);
        // A click anywhere but on a machine releases the machine filter.
        if click
            && self.screen == Screen::Simulation
            && !matches!(target, Some(HitTarget::Machine(_)))
        {
            if let Some(focus) = self.focus.as_mut() {
                if focus.selected.take().is_some() {
                    self.logs_scroll = 0;
                }
            }
        }
        let Some(target) = target else {
            return Vec::new();
        };
        let up = mouse.kind == MouseEventKind::ScrollUp;
        let down = mouse.kind == MouseEventKind::ScrollDown;
        if up || down {
            match target {
                HitTarget::Logs if self.events_tab => {
                    self.events_scroll = if up {
                        self.events_scroll
                            .saturating_add(3)
                            .min(self.focus.as_ref().map_or(0, |f| f.events.len()))
                    } else {
                        self.events_scroll.saturating_sub(3)
                    }
                }
                HitTarget::Logs => {
                    let width = self
                        .hitboxes
                        .borrow()
                        .iter()
                        .find(|(_, target)| *target == HitTarget::Logs)
                        .map_or(80, |(rect, _)| rect.width.saturating_sub(2));
                    let maximum = self
                        .focus
                        .as_ref()
                        .map_or(0, |focus| crate::view::log_rows(focus, width).len());
                    self.logs_scroll = if up {
                        self.logs_scroll.saturating_add(3).min(maximum)
                    } else {
                        self.logs_scroll.saturating_sub(3)
                    };
                }
                HitTarget::Fault(_) | HitTarget::Faults => {
                    self.panel = Panel::Faults;
                    self.move_fault_selection(if up { -1 } else { 1 });
                }
                HitTarget::Timeline(_) => {
                    return self.timeline_key(KeyEvent::new(
                        if up { KeyCode::Up } else { KeyCode::Down },
                        KeyModifiers::NONE,
                    ))
                }
                HitTarget::Catalogue(_) => {
                    return self.catalogue_key(KeyEvent::new(
                        if up { KeyCode::Up } else { KeyCode::Down },
                        KeyModifiers::NONE,
                    ))
                }
                HitTarget::Machine(_) => {
                    if let Some(focus) = self.focus.as_mut() {
                        select(focus, if up { -1 } else { 1 });
                    }
                }
                _ => {}
            }
        } else if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            match target {
                HitTarget::Machine(_) => self.panel = Panel::Machines,
                HitTarget::Fault(_) | HitTarget::Faults => self.panel = Panel::Faults,
                HitTarget::Logs => self.panel = Panel::Logs,
                _ => {}
            }
            match target {
                HitTarget::Screen(screen) => {
                    return self.input_raw(Event::Key(KeyEvent::new(
                        KeyCode::Char(match screen {
                            Screen::Catalogue => '1',
                            Screen::Simulation => '2',
                            Screen::Timeline => '3',
                        }),
                        KeyModifiers::NONE,
                    )))
                }
                HitTarget::Catalogue(row) => {
                    self.list_selected = row.min(self.tree().len().saturating_sub(1))
                }
                HitTarget::Timeline(row) => {
                    self.timeline_row = row.min(
                        self.timelines
                            .get(self.timeline_source)
                            .map_or(0, |d| d.entries().len().saturating_sub(1)),
                    )
                }
                HitTarget::Machine(row) => {
                    if let Some(focus) = self.focus.as_mut() {
                        focus.selected = focus
                            .world
                            .as_ref()
                            .and_then(|world| world.machines.get(row))
                            .map(|machine| machine.name.clone());
                        self.logs_scroll = 0;
                    }
                }
                HitTarget::Fault(row) => {
                    if let Some(focus) = self.focus.as_mut() {
                        let activity = focus.activity();
                        focus.fault_selected =
                            activity.rows().nth(row).map(|item| item.key.clone());
                    }
                }
                _ => {}
            }
        }
        Vec::new()
    }

    /// Move the faults panel selection through active, upcoming and history rows.
    fn move_fault_selection(&mut self, step: isize) {
        let Some(focus) = self.focus.as_mut() else {
            return;
        };
        let activity = focus.activity();
        let keys: Vec<Key> = activity.rows().map(|item| item.key.clone()).collect();
        if keys.is_empty() {
            focus.fault_selected = None;
            return;
        }
        let current = focus
            .fault_selected
            .as_ref()
            .and_then(|key| keys.iter().position(|k| k == key));
        let next = match current {
            None if step < 0 => keys.len() - 1,
            None => 0,
            Some(index) => (index as isize + step).clamp(0, keys.len() as isize - 1) as usize,
        };
        focus.fault_selected = Some(keys[next].clone());
    }

    /// A rerun creates a new simulation, so it takes a second `r` to confirm.
    fn rerun(&mut self, id: SimulationId) -> Vec<Request> {
        if self.confirm_rerun.as_ref() == Some(&id) {
            self.confirm_rerun = None;
            self.notify(format!("submitting a rerun of {id}…"), Tone::Info);
            return vec![Request::Rerun(id)];
        }
        self.notify(
            format!("press r again to rerun {id} as a new simulation"),
            Tone::Info,
        );
        self.confirm_rerun = Some(id);
        Vec::new()
    }

    fn catalogue_key(&mut self, key: KeyEvent) -> Vec<Request> {
        let rows = self.tree().len();
        let selected = self.selected_summary();
        if key.code != KeyCode::Char('d') {
            self.confirm_delete = None;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.list_selected = (self.list_selected + 1).min(rows.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.list_selected = self.list_selected.saturating_sub(1)
            }
            KeyCode::Enter => {
                if let Some(id) = selected {
                    return self.open(id);
                }
            }
            KeyCode::Char('r') => {
                if let Some(id) = selected {
                    return self.rerun(id);
                }
            }
            KeyCode::Char('b') => return selected.map(Request::Branch).into_iter().collect(),
            KeyCode::Char('s') => return selected.map(Request::Stop).into_iter().collect(),
            KeyCode::Char('m') => {
                if let Some(id) = selected {
                    self.notify(
                        format!("baseline {id}; select another and press c"),
                        Tone::Info,
                    );
                    self.baseline = Some(id);
                }
            }
            KeyCode::Char('c') => match (self.baseline.clone(), selected) {
                (Some(baseline), Some(id)) if baseline != id => {
                    return vec![Request::Compare(baseline, id)];
                }
                _ => self.notify("mark a baseline with m first", Tone::Info),
            },
            KeyCode::Char('d') => {
                if let Some(id) = selected {
                    if self.confirm_delete.as_ref() == Some(&id) {
                        self.confirm_delete = None;
                        return vec![Request::Delete(id)];
                    }
                    self.notify(
                        format!("press d again to delete {id} and its branches"),
                        Tone::Bad,
                    );
                    self.confirm_delete = Some(id);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn open(&mut self, id: SimulationId) -> Vec<Request> {
        self.screen = Screen::Simulation;
        if self.focus.as_ref().is_some_and(|f| f.id == id) {
            return Vec::new();
        }
        self.fault_form = None;
        self.focus = Some(Focus::new(id.clone()));
        self.inspector = None;
        self.logs_scroll = 0;
        self.events_scroll = 0;
        self.zoom = 0;
        vec![Request::Open(id)]
    }

    fn simulation_key(&mut self, key: KeyEvent) -> Vec<Request> {
        let Some(focus) = self.focus.as_mut() else {
            return Vec::new();
        };
        let live = matches!(
            key.code,
            KeyCode::Char(' ' | '.' | '+' | '=' | '-' | 'c' | 'f' | ':' | 'h' | 'n')
        );
        if focus.terminal() && live {
            self.notify(
                "simulation completed; r reruns it, 3 edits its timeline",
                Tone::Info,
            );
            return Vec::new();
        }
        if live && !focus.executing() {
            self.notify(
                "playback controls start once the simulation is executing",
                Tone::Info,
            );
            return Vec::new();
        }
        let id = focus.id.clone();
        let control = |command| vec![Request::Control(id.clone(), command)];
        if self.control_pending && live {
            self.notify("waiting for playback control acknowledgement", Tone::Info);
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc => self.screen = Screen::Catalogue,
            KeyCode::Char('a') => focus.selected = None,
            KeyCode::Tab | KeyCode::BackTab => {
                let order = [Panel::Machines, Panel::Faults, Panel::Logs];
                let index = order.iter().position(|p| *p == self.panel).unwrap_or(0);
                let step = if key.code == KeyCode::Tab { 1 } else { 2 };
                self.panel = order[(index + step) % order.len()];
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Up | KeyCode::Char('k') => {
                let step = if matches!(key.code, KeyCode::Down | KeyCode::Char('j')) {
                    1
                } else {
                    -1
                };
                match self.panel {
                    Panel::Machines => select(focus, step),
                    Panel::Faults => self.move_fault_selection(step),
                    Panel::Logs => {
                        let offset = if self.events_tab {
                            &mut self.events_scroll
                        } else {
                            &mut self.logs_scroll
                        };
                        *offset = offset.saturating_add_signed(-step);
                    }
                }
            }
            KeyCode::Char('e') => {
                self.events_tab = !self.events_tab;
                self.panel = Panel::Logs;
            }
            KeyCode::Char('[') => self.zoom = self.zoom.saturating_sub(1),
            KeyCode::Char(']') => self.zoom = (self.zoom + 1).min(12),
            KeyCode::Char(' ') => {
                let rate = if focus.paused() {
                    focus.resume_rate
                } else {
                    PlaybackRate::Paused
                };
                return control(ControlCommand::SetRate { rate });
            }
            KeyCode::Char('.') => {
                if !focus.paused() {
                    return vec![Request::PauseThenStep(id.clone())];
                }
                return control(ControlCommand::Step);
            }
            KeyCode::Char('n') => {
                let activity = focus.activity();
                if activity.next(focus.now_ns(Instant::now())).is_none() {
                    self.notify(
                        "no upcoming fault in this simulation's timeline",
                        Tone::Info,
                    );
                    return Vec::new();
                }
                let rate = if focus.paused() {
                    focus.resume_rate
                } else {
                    focus
                        .playback
                        .as_ref()
                        .map_or(focus.resume_rate, |(p, _)| p.state.rate)
                };
                return control(ControlCommand::PauseBeforeNextFault { rate });
            }
            KeyCode::Char('h') => {
                let activity = focus.activity();
                let Some(handle) = focus.healable(&activity) else {
                    self.notify("no active fault with a handle to heal", Tone::Info);
                    return Vec::new();
                };
                let action = serde_json::json!({"action": "heal", "handle": handle});
                let admissible = focus.fault_types.as_ref().and_then(|catalogue| {
                    catalogue
                        .faults
                        .iter()
                        .find(|fault| fault.name == "heal")
                        .map(|fault| crate::fault_form::validate(&fault.schema, &action, "heal"))
                });
                return match admissible {
                    Some(Ok(())) => vec![Request::Heal(id.clone(), action)],
                    Some(Err(error)) => {
                        self.notify(format!("cannot heal {handle}: {error}"), Tone::Bad);
                        Vec::new()
                    }
                    None => {
                        self.notify(
                            "this simulation's fault catalogue offers no heal",
                            Tone::Info,
                        );
                        Vec::new()
                    }
                };
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') => {
                let current = if focus.paused() {
                    focus.resume_rate
                } else {
                    focus
                        .playback
                        .as_ref()
                        .map_or(PlaybackRate::Unlimited, |(p, _)| p.state.rate)
                };
                let index = rate_index(current);
                let next = if key.code == KeyCode::Char('-') {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(RATES.len())
                };
                focus.resume_rate = ladder(next);
                if focus.paused() {
                    self.notify(
                        format!("resume rate {}", rate_label(ladder(next))),
                        Tone::Info,
                    );
                    return Vec::new();
                }
                return control(ControlCommand::SetRate { rate: ladder(next) });
            }
            KeyCode::Char('c') => return control(ControlCommand::Checkpoint),
            KeyCode::Char('b') => {
                self.notify("branching from the newest checkpoint…", Tone::Info);
                return vec![Request::Branch(id.clone())];
            }
            KeyCode::Char('r') => return self.rerun(id),
            KeyCode::Char('f') | KeyCode::Char(':') => {
                if focus.fault_types.is_some() {
                    self.typeahead.clear();
                    self.fault_menu = Some(FaultMenu::Opening);
                    return vec![Request::BeginFaultMenu(id)];
                } else {
                    self.notify("fault catalogue is loading or unavailable", Tone::Info);
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

fn select(focus: &mut Focus, step: isize) {
    let machines = focus.machines();
    // The first position is the unfiltered, all-machine view.
    let current = focus
        .selected
        .as_ref()
        .and_then(|s| machines.iter().position(|m| m == s))
        .map_or(0, |i| i + 1) as isize;
    let next = (current + step).rem_euclid((machines.len() + 1) as isize) as usize;
    focus.selected = next.checked_sub(1).map(|i| machines[i].clone());
}

/// The simulation a run was derived from: its parent, else its lineage annotation.
pub fn source_of(s: &SimulationSummary) -> Option<SimulationId> {
    s.metadata.parent.clone().or_else(|| {
        s.metadata
            .annotations
            .get("derived-from")
            .and_then(|id| id.parse().ok())
    })
}

pub fn rate_label(rate: PlaybackRate) -> String {
    match rate {
        PlaybackRate::Paused => "paused".into(),
        PlaybackRate::Unlimited => "unlimited".into(),
        PlaybackRate::Limited {
            numerator,
            denominator,
        } => match (numerator.get(), denominator.get()) {
            (n, 1) => format!("{n}×"),
            (1, d) => format!("1/{d}×"),
            (n, d) => format!("{n}/{d}×"),
        },
    }
}

/// Project the admitted opaque payload immediately, before a user can quit.
fn receipt_timeline_entry(receipt: &serde_json::Value) -> Option<seedbox_api::TimelineEntry> {
    let intervention = receipt.get("state")?.get("intervention")?;
    let plan = intervention.get("plan")?;
    let mut action = plan.clone();
    let fields = action.as_object_mut()?;
    fields.remove("id");
    fields.remove("at_ns");
    fields.remove("deadline_ns");
    Some(seedbox_api::TimelineEntry {
        id: format!("program:{}", intervention.get("requestId")?.as_str()?),
        at_ns: plan.get("at_ns")?.as_u64()?,
        action,
        origin: seedbox_api::TimelineOrigin::Program,
        position: serde_json::from_value(intervention.get("position")?.clone()).ok(),
        fault_id: plan
            .get("id")?
            .as_u64()
            .and_then(|id| u32::try_from(id).ok()),
    })
}

fn describe_receipt(receipt: &ControlReceipt) -> String {
    use seedbox_api::ControlState;
    match &receipt.state {
        ControlState::Accepted => "accepted; waiting for the next boundary".into(),
        ControlState::Unsupported => "unsupported by this worker".into(),
        ControlState::Interrupted => "interrupted before acknowledgement".into(),
        ControlState::Acknowledged {
            status,
            error,
            intervention,
            detail,
            checkpoint,
        } => {
            let mut text = format!(
                "acknowledged at boundary {} (round {}, {})",
                status.position.boundary_index,
                status.position.completed_rounds,
                crate::view::time(status.position.simulated_time_ns)
            );
            if let Some(error) = error {
                text.push_str(&format!(" · error {error:?}"));
            }
            if let Some(intervention) = intervention {
                text.push_str(&format!(" · fault #{}", intervention.plan.id));
            }
            if let Some(checkpoint) = checkpoint {
                text.push_str(&format!(
                    " · checkpoint {} {:?}",
                    checkpoint.request_id, checkpoint.state
                ));
            }
            if let Some(detail) = detail {
                text.push_str(&format!(" · {detail}"));
            }
            text
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use seedbox_api::{LinkCounters, LinkPolicy, LinkState, MachineCondition, MachineState};

    pub(crate) fn world(names: &[&str]) -> WorldState {
        let machines = names
            .iter()
            .map(|n| MachineState {
                name: (*n).into(),
                incarnation: 0,
                condition: MachineCondition::Ready,
                exit_code: None,
                throttle_speed_hz: None,
                oracle: *n == "oracle",
                last_transition: None,
                last_transition_at_ns: None,
                condition_since_ns: None,
                application: Default::default(),
            })
            .collect();
        let mut links = Vec::new();
        for a in names {
            for b in names {
                if a != b {
                    links.push(LinkState {
                        from: (*a).into(),
                        to: (*b).into(),
                        policy: LinkPolicy {
                            latency_ns: 1_000_000,
                            jitter_ns: 0,
                            loss_ppm: 0,
                            bandwidth_bps: None,
                        },
                        partitioned_by: None,
                        shaped_by: None,
                        counters: LinkCounters::default(),
                    });
                }
            }
        }
        WorldState {
            override_rejections: Default::default(),
            simulation: "k7m2p9qx".parse().unwrap(),
            revision: 1,
            simulated_time_ns: 0,
            machines,
            links,
            faults: Vec::new(),
        }
    }

    pub(crate) fn app(names: &[&str]) -> App {
        let mut app = App::new(Some("k7m2p9qx".parse().unwrap()));
        app.apply(Update::World(world(names)));
        app.focus.as_mut().unwrap().selected = names.first().map(|name| (*name).to_string());
        app
    }

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn completed_simulation_rejects_live_controls_but_allows_rerun() {
        let mut app = app(&["a"]);
        app.apply(Update::Playback(PlaybackObservation {
            state: seedbox_api::PlaybackStatus {
                position: Default::default(),
                rate: PlaybackRate::Unlimited,
                achieved_rate: None,
                stepping: false,
                terminal: true,
                application_ready: true,
            },
            age_ns: 0,
        }));
        app.fault_menu = Some(FaultMenu::Opening);
        assert!(app
            .apply(Update::FaultMenuReady(Ok(PlaybackRate::Unlimited)))
            .is_empty());
        assert!(app.fault_menu.is_none());
        for c in [' ', '.', '+', '=', '-', 'c', 'f', ':'] {
            assert!(app.input(key(c)).is_empty());
            assert!(app
                .notice(Instant::now())
                .unwrap()
                .text
                .contains("simulation completed"));
        }
        assert!(app.input(key('r')).is_empty());
        assert!(app.notice.as_ref().unwrap().text.contains("press r again"));
        assert_eq!(
            app.input(key('r')),
            vec![Request::Rerun("k7m2p9qx".parse().unwrap())]
        );
        // Any other key cancels a pending confirmation.
        app.input(key('r'));
        app.input(key('a'));
        assert!(app.input(key('r')).is_empty());
    }

    #[test]
    fn unfiltered_selection_survives_refresh_and_navigation_cycles_through_it() {
        let mut app = App::new(Some("k7m2p9qx".parse().unwrap()));
        app.apply(Update::World(world(&["a", "b"])));
        assert_eq!(app.focus.as_ref().unwrap().selected, None);
        app.input(key('j'));
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("a"));
        app.input(key('j'));
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("b"));
        app.input(key('j'));
        assert_eq!(app.focus.as_ref().unwrap().selected, None);
        app.input(key('k'));
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("b"));
        app.input(key('a'));
        app.apply(Update::World(world(&["a", "b", "late"])));
        assert_eq!(app.focus.as_ref().unwrap().selected, None);
    }

    #[test]
    fn machines_come_and_go_with_the_world() {
        let mut app = app(&["a", "b", "oracle"]);
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("a"));
        app.apply(Update::World(world(&["b", "late", "oracle"])));
        let focus = app.focus.as_ref().unwrap();
        assert_eq!(focus.machines(), ["b", "late", "oracle"]);
        assert_eq!(focus.selected, None);
    }

    #[test]
    fn older_world_responses_cannot_restore_removed_machines() {
        let mut app = app(&["a", "b"]);
        let mut newer = world(&["b", "late"]);
        newer.revision = 3;
        newer.simulated_time_ns = 50;
        app.apply(Update::World(newer));
        app.apply(Update::World(world(&["a", "b"])));
        let focus = app.focus.as_ref().unwrap();
        assert_eq!(focus.machines(), ["b", "late"]);
        assert_eq!(focus.selected, None);
        assert_eq!(focus.now_ns(Instant::now()), 50);
        let mut empty = world(&[]);
        empty.revision = 4;
        app.apply(Update::World(empty));
        assert!(app.focus.as_ref().unwrap().selected.is_none());
    }

    #[test]
    fn old_focus_feeds_cannot_change_the_current_world() {
        let mut app = app(&["a"]);
        let other = "a7m2p9qx".parse().unwrap();
        app.apply(Update::Focused(
            other,
            Box::new(Update::Checkpoints(vec![Checkpoint {
                name: "old".into(),
                at_ns: 1,
            }])),
        ));
        assert!(app.focus.as_ref().unwrap().checkpoints.is_empty());
        app.apply(Update::Focused(
            "k7m2p9qx".parse().unwrap(),
            Box::new(Update::Checkpoints(vec![Checkpoint {
                name: "current".into(),
                at_ns: 2,
            }])),
        ));
        assert_eq!(app.focus.as_ref().unwrap().checkpoints[0].name, "current");
    }

    #[test]
    fn missing_server_resource_closes_inspection() {
        let mut app = app(&["a"]);
        let requests = app.apply(Update::Missing("k7m2p9qx".parse().unwrap()));
        assert_eq!(requests, [Request::Close]);
        assert!(app.focus.is_none());
        assert_eq!(app.screen, Screen::Catalogue);
    }

    #[test]
    fn simulated_time_never_extrapolates_past_server_observations() {
        let mut app = app(&["a"]);
        let focus = app.focus.as_mut().unwrap();
        focus.playback = Some((
            PlaybackObservation {
                state: seedbox_api::PlaybackStatus {
                    position: seedbox_api::BoundaryPosition {
                        simulated_time_ns: 25,
                        ..Default::default()
                    },
                    rate: limited(1, 1),
                    achieved_rate: None,
                    stepping: false,
                    terminal: false,
                    application_ready: true,
                },
                age_ns: 0,
            },
            Instant::now(),
        ));
        assert_eq!(focus.now_ns(Instant::now() + Duration::from_secs(20)), 25);
    }

    #[test]
    fn unknown_machines_in_logs_wait_for_the_observed_world() {
        let mut app = app(&["a"]);
        let requests = app.apply(Update::Log(
            "k7m2p9qx".parse().unwrap(),
            LogDelivery::Entry {
                sequence: 1,
                entry: LogEntry::Bytes {
                    machine: "new".into(),
                    incarnation: 0,
                    simulated_time_ns: 5,
                    stream: LogStream::Stdout,
                    bytes: b"hello\nwor".to_vec(),
                },
            },
        ));
        assert!(requests.is_empty());
        let log = &app.focus.as_ref().unwrap().logs["new"];
        assert_eq!(log.lines.len(), 1);
        assert_eq!(log.lines[0].text, "hello");
    }

    fn output(sequence: u64, bytes: &[u8]) -> Update {
        Update::Log(
            "k7m2p9qx".parse().unwrap(),
            LogDelivery::Entry {
                sequence,
                entry: LogEntry::Bytes {
                    machine: "a".into(),
                    incarnation: 0,
                    simulated_time_ns: 5,
                    stream: LogStream::Stdout,
                    bytes: bytes.to_vec(),
                },
            },
        )
    }

    #[test]
    fn scrolled_logs_anchor_wrapped_rows_until_end_restores_follow() {
        let mut app = app(&["a"]);
        app.hitboxes
            .borrow_mut()
            .push((ratatui::layout::Rect::new(0, 0, 42, 8), HitTarget::Logs));
        app.apply(output(1, b"first\nsecond\nthird\n"));
        app.input(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
        assert!(app.logs_scroll > 0);
        let before = app.logs_scroll;
        let rows_before = crate::view::log_rows(app.focus.as_ref().unwrap(), 40).len();
        app.apply(output(
            2,
            b"a long message that wraps beyond the available width into multiple terminal rows\n",
        ));
        let added = crate::view::log_rows(app.focus.as_ref().unwrap(), 40).len() - rows_before;
        assert!(added > 1);
        assert_eq!(app.logs_scroll, before + added);
        app.input(Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
        assert_eq!(app.logs_scroll, 0);
        app.apply(output(3, b"new live output\n"));
        assert_eq!(app.logs_scroll, 0);
    }

    #[test]
    fn timeline_navigation_never_pauses_or_retains_log_keyboard_focus() {
        for paused in [false, true] {
            let mut app = edited_timeline_app();
            app.screen = Screen::Simulation;
            app.panel = Panel::Logs;
            app.focus.as_mut().unwrap().playback = Some((
                PlaybackObservation {
                    state: seedbox_api::PlaybackStatus {
                        position: Default::default(),
                        rate: if paused {
                            PlaybackRate::Paused
                        } else {
                            PlaybackRate::Unlimited
                        },
                        achieved_rate: None,
                        stepping: false,
                        terminal: false,
                        application_ready: true,
                    },
                    age_ns: 0,
                },
                Instant::now(),
            ));
            assert!(app.input(key('3')).is_empty());
            assert_eq!(app.screen, Screen::Timeline);
            assert_eq!(app.fault_menu, None);
            app.input(key('d'));
            assert_eq!(app.timelines[0].modified_count(), 1);
            assert!(app.input(key('2')).is_empty());
            assert_eq!(app.screen, Screen::Simulation);
            app.control_pending = true;
            assert!(app.input(key('3')).is_empty());
            assert_eq!(app.screen, Screen::Timeline);
            assert!(app.input(key('1')).is_empty());
            assert_eq!(app.screen, Screen::Catalogue);
        }
    }

    #[test]
    fn unterminated_output_stays_within_the_line_limit() {
        let mut log = MachineLog::default();
        for _ in 0..MAX_LOG_LINES + 10 {
            log.push(0, LogStream::Stdout, &[b'x'; 4097]);
        }
        assert_eq!(log.lines.len(), MAX_LOG_LINES);
    }

    #[test]
    fn reconnect_history_does_not_duplicate_lines_or_partial_bytes() {
        let mut app = app(&["a"]);
        app.apply(output(1, b"hello\npar"));
        app.apply(output(1, b"hello\npar"));
        app.apply(output(2, b"tial\n"));
        let log = &app.focus.as_ref().unwrap().logs["a"];
        assert_eq!(
            log.lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["hello", "partial"]
        );
    }

    #[test]
    fn log_updates_are_scoped_to_the_open_simulation() {
        let mut app = app(&["a"]);
        app.open("a7m2p9qx".parse().unwrap());
        app.apply(output(1, b"old simulation\n"));
        assert!(app.focus.as_ref().unwrap().logs.is_empty());
        app.open("k7m2p9qx".parse().unwrap());
        app.apply(output(1, b"new subscription\n"));
        assert_eq!(
            app.focus.as_ref().unwrap().logs["a"].lines[0].text,
            "new subscription"
        );
    }

    #[test]
    fn eviction_gaps_are_visible_and_do_not_join_unrelated_bytes() {
        let mut app = app(&["a"]);
        app.apply(output(1, b"unfinished"));
        let gap = || {
            Update::Log(
                "k7m2p9qx".parse().unwrap(),
                LogDelivery::Gap { first: 1, last: 4 },
            )
        };
        app.apply(gap());
        app.apply(gap());
        app.apply(output(2, b"replayed\n"));
        app.apply(output(5, b"fresh\n"));
        let focus = app.focus.as_ref().unwrap();
        assert_eq!(focus.lost, 3);
        let log = &focus.logs["a"];
        assert_eq!(log.lines.len(), 2);
        assert_eq!(log.lines[0].text, "[3 output records were lost]");
        assert_eq!(log.lines[1].text, "fresh");
    }

    #[test]
    fn worker_gaps_and_unavailable_output_clear_partial_lines() {
        let mut app = app(&["a", "b"]);
        app.apply(output(1, b"unfinished"));
        app.apply(Update::Log(
            "k7m2p9qx".parse().unwrap(),
            LogDelivery::Entry {
                sequence: 2,
                entry: LogEntry::SourceGap { first: 7, last: 8 },
            },
        ));
        app.apply(output(3, b"fresh\nunfinished"));
        app.apply(Update::Log(
            "k7m2p9qx".parse().unwrap(),
            LogDelivery::Entry {
                sequence: 4,
                entry: LogEntry::SourceUnavailable,
            },
        ));
        app.apply(output(5, b"last\n"));
        let focus = app.focus.as_ref().unwrap();
        assert_eq!(focus.lost, 2);
        let texts: Vec<_> = focus.logs["a"]
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            texts,
            [
                "[2 worker output records were lost]",
                "fresh",
                "[output source unavailable]",
                "last"
            ]
        );
        assert_eq!(focus.logs["b"].lines.len(), 2);
    }

    #[test]
    fn catalogue_sorts_newest_first_without_changing_the_selected_id() {
        let summary = |id: &str, created| seedbox_api::SimulationSummary {
            metadata: seedbox_api::Metadata {
                name: "test".into(),
                labels: Default::default(),
                annotations: Default::default(),
                id: id.parse().unwrap(),
                revision: 1,
                created_at_unix_ms: created,
                deletion_requested_at_unix_ms: None,
                parent: None,
                checkpoint: None,
            },
            status: Default::default(),
            oracle: "oracle".into(),
        };
        let mut app = App::new(None);
        app.apply(Update::List(vec![summary("k7m2p9qx", 10)]));
        app.apply(Update::List(vec![
            summary("k7m2p9qx", 10),
            summary("a7m2p9qx", 20),
        ]));
        assert_eq!(app.list[0].metadata.id.as_str(), "a7m2p9qx");
        assert_eq!(app.selected_summary().unwrap().as_str(), "k7m2p9qx");
    }

    fn fault_catalogue(action: &str) -> FaultCatalogue {
        FaultCatalogue {
            simulation: "k7m2p9qx".parse().unwrap(),
            faults: vec![seedbox_api::FaultDescriptor {
                name: action.into(),
                title: "Server fault".into(),
                description: "Future behavior".into(),
                schema: serde_json::json!({"type":"object","required":["action","handle"],"properties":{
                    "action":{"const":action},"handle":{"type":"string","default":"split"}
                }}),
            }],
        }
    }

    fn submit_fault(app: &mut App) -> Vec<Request> {
        app.input(key('f'));
        if app.fault_menu == Some(FaultMenu::Opening) {
            app.apply(Update::FaultMenuReady(Ok(PlaybackRate::Paused)));
        }
        app.input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        app.input(Event::Key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )));
        app.input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
    }

    #[test]
    fn rejected_fault_keeps_values_and_restores_only_on_cancel() {
        let mut app = app(&["a"]);
        app.apply(Update::FaultTypes(fault_catalogue("future_server_fault")));
        app.input(key('f'));
        let prior = limited(3, 7);
        app.apply(Update::FaultMenuReady(Ok(prior)));
        app.input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        app.input(key('x'));
        app.input(Event::Key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )));
        let submit = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let first = app.input(submit.clone());
        assert_eq!(first.len(), 1);
        assert!(app.fault_form.is_some());
        app.apply(Update::FaultRejected(prior, "invalid target".into()));
        assert_eq!(app.input(submit), first);
        app.apply(Update::FaultRejected(prior, "invalid target".into()));
        assert_eq!(
            app.cancel_fault_menu(),
            [Request::EndFaultMenu("k7m2p9qx".parse().unwrap(), prior)]
        );
    }

    #[test]
    fn foreign_saved_timeline_never_controls_focused_simulation() {
        let mut app = app(&["a"]);
        app.screen = Screen::Timeline;
        app.timelines.push(crate::timeline::TimelineDraft::new(
            "a7m2p9qx".parse().unwrap(),
            10,
        ));
        assert!(app.input(key(' ')).is_empty());
        app.fault_menu = Some(FaultMenu::Editing(PlaybackRate::Unlimited));
        assert!(app.input(key(' ')).is_empty());
        assert_eq!(
            app.fault_menu,
            Some(FaultMenu::Editing(PlaybackRate::Unlimited))
        );
        assert_eq!(
            app.cancel_fault_menu(),
            [Request::EndFaultMenu(
                "k7m2p9qx".parse().unwrap(),
                PlaybackRate::Unlimited
            )]
        );
    }

    #[test]
    fn server_catalogue_form_submits_unknown_action_as_json() {
        let mut app = app(&["a"]);
        app.apply(Update::FaultTypes(fault_catalogue("future_server_fault")));
        assert_eq!(
            submit_fault(&mut app),
            vec![Request::Fault(
                "k7m2p9qx".parse().unwrap(),
                serde_json::json!({"action":"future_server_fault","handle":"split"}),
                PlaybackRate::Paused,
            )]
        );
    }

    #[test]
    fn schema_heal_submission_is_deduplicated_while_paused() {
        let mut app = app(&["a", "b"]);
        app.apply(Update::FaultTypes(fault_catalogue("heal")));
        app.apply(Update::Playback(PlaybackObservation {
            state: seedbox_api::PlaybackStatus {
                position: Default::default(),
                rate: PlaybackRate::Paused,
                achieved_rate: None,
                stepping: false,
                terminal: false,
                application_ready: true,
            },
            age_ns: 0,
        }));
        assert_eq!(submit_fault(&mut app).len(), 1);
        assert!(submit_fault(&mut app).is_empty());
        assert_eq!(app.fault_menu, Some(FaultMenu::Finishing));
        app.apply(Update::FaultReceipt { title: "heal".into(),
            request: serde_json::json!({"command":{"type":"injectFault","action":{"action":"heal","handle":"split"}}}).to_string(),
            result: Err("request rejected".into()),
        });
        app.apply(Update::FaultMenuDone(false));
        app.input(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(submit_fault(&mut app).len(), 1);
    }

    #[test]
    fn fault_menu_waits_for_pause_then_cancel_restores_exact_rate() {
        let mut app = app(&["a"]);
        app.apply(Update::FaultTypes(fault_catalogue("stop")));
        assert_eq!(
            app.input(key('f')),
            [Request::BeginFaultMenu("k7m2p9qx".parse().unwrap())]
        );
        assert!(app.fault_form.is_none());
        assert!(app.input(key('f')).is_empty());
        assert!(app.input(key(' ')).is_empty());
        let prior = limited(3, 7);
        app.apply(Update::FaultMenuReady(Ok(prior)));
        assert!(app.fault_form.is_some());
        let requests = app.input(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(
            requests,
            [Request::EndFaultMenu("k7m2p9qx".parse().unwrap(), prior)]
        );
        assert_eq!(app.fault_menu, Some(FaultMenu::Finishing));
        app.apply(Update::FaultMenuDone(true));
        assert_eq!(app.fault_menu, None);
    }

    #[test]
    fn quitting_while_pause_pending_waits_then_restores() {
        let mut app = app(&["a"]);
        app.apply(Update::FaultTypes(fault_catalogue("stop")));
        app.input(key('f'));
        app.input(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!app.quit);
        let requests = app.apply(Update::FaultMenuReady(Ok(PlaybackRate::Unlimited)));
        assert_eq!(
            requests,
            [Request::EndFaultMenu(
                "k7m2p9qx".parse().unwrap(),
                PlaybackRate::Unlimited
            )]
        );
        assert!(!app.quit);
        app.apply(Update::FaultMenuDone(true));
        assert!(app.quit);
    }

    #[test]
    fn failed_admission_requires_dismissal_and_never_restores() {
        let mut app = app(&["a"]);
        app.fault_menu = Some(FaultMenu::Finishing);
        assert!(app.apply(Update::FaultMenuDone(false)).is_empty());
        assert_eq!(app.fault_menu, Some(FaultMenu::Failed));
        assert!(app.input(key(' ')).is_empty());
        app.input(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(app.fault_menu, None);
    }

    #[test]
    fn fault_menu_cannot_race_an_existing_playback_control() {
        let mut app = app(&["a"]);
        app.apply(Update::FaultTypes(fault_catalogue("stop")));
        app.input(key(' '));
        assert!(app.input(key('f')).is_empty());
        app.apply(Update::ControlDone);
        assert_eq!(app.input(key('f')).len(), 1);
    }

    #[test]
    fn step_pauses_a_running_simulation_first() {
        let mut app = app(&["a"]);
        let requests = app.input(key('.'));
        assert_eq!(
            requests,
            [Request::PauseThenStep("k7m2p9qx".parse().unwrap())]
        );
    }

    #[test]
    fn rate_ladder_moves_between_limited_and_unlimited() {
        assert_eq!(rate_index(PlaybackRate::Unlimited), RATES.len());
        assert_eq!(rate_index(limited(1, 200)), 1);
        assert_eq!(ladder(RATES.len()), PlaybackRate::Unlimited);
        assert_eq!(ladder(0), limited(1, 1000));
    }

    #[test]
    fn frames_leave_once_delivered() {
        let mut app = app(&["a", "b"]);
        let focus = app.focus.as_mut().unwrap();
        focus.frames.push_back(Frame {
            from: "a".into(),
            to: "b".into(),
            sent_ns: 10,
            due_ns: Some(1_000_010),
            bytes: 60,
            fate: FrameFate::Scheduled,
        });
        focus.prune_frames(500_000);
        assert_eq!(focus.frames.len(), 1);
        focus.prune_frames(1_000_011);
        assert!(focus.frames.is_empty());
    }
    pub(crate) fn timeline_resource(created: u64) -> seedbox_api::Simulation {
        let spec = serde_json::from_value(serde_json::json!({
            "seed":42,"oracle":{"image":"example/oracle:latest"}
        }))
        .unwrap();
        let mut resource = seedbox_api::Simulation::queued(
            seedbox_api::Metadata {
                name: "test".into(),
                labels: Default::default(),
                annotations: Default::default(),
                id: "k7m2p9qx".parse().unwrap(),
                revision: 1,
                created_at_unix_ms: created,
                deletion_requested_at_unix_ms: None,
                parent: None,
                checkpoint: None,
            },
            spec,
        );
        resource.status.phase = seedbox_api::Phase::Completed;
        resource
    }

    fn edited_timeline_app() -> App {
        let mut app = app(&["a"]);
        app.focus.as_mut().unwrap().resource = Some(timeline_resource(10));
        app.screen = Screen::Timeline;
        let mut draft = crate::timeline::TimelineDraft::new("k7m2p9qx".parse().unwrap(), 10);
        let entries = [
            seedbox_api::TimelineEntry {
                id: "first".into(),
                at_ns: 100,
                action: serde_json::json!({"action":"future_stop","machine":"a"}),
                origin: seedbox_api::TimelineOrigin::Program,
                position: None,
                fault_id: Some(1),
            },
            seedbox_api::TimelineEntry {
                id: "second".into(),
                at_ns: 200,
                action: serde_json::json!({"action":"future_restart","machine":"a"}),
                origin: seedbox_api::TimelineOrigin::Program,
                position: None,
                fault_id: Some(2),
            },
        ];
        draft.merge(&entries).unwrap();
        app.timelines.push(draft);
        app
    }

    #[test]
    fn timeline_application_name_comes_from_resource_metadata() {
        let mut app = edited_timeline_app();
        let resource = app.focus.as_mut().unwrap().resource.as_mut().unwrap();
        resource.metadata.name = "resolved application".into();
        resource.spec.configuration.name.clear();
        let entries = app.timelines[0].entries().to_vec();
        app.timeline_update(seedbox_api::FaultTimeline {
            simulation: "k7m2p9qx".parse().unwrap(),
            entries,
        });
        assert_eq!(
            app.timelines[0].metadata().application,
            "resolved application"
        );
        assert_eq!(app.timelines[0].metadata().name, "resolved application");

        app.timelines[0].rename("my recording".into());
        let entries = app.timelines[0].entries().to_vec();
        app.timeline_update(seedbox_api::FaultTimeline {
            simulation: "k7m2p9qx".parse().unwrap(),
            entries,
        });
        assert_eq!(app.timelines[0].metadata().name, "my recording");
    }

    #[test]
    fn edited_submission_requires_complete_definition_instead_of_source_lookup() {
        let mut app = edited_timeline_app();
        app.input(key('d'));
        let requests = app.input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(requests.is_empty());
        assert_eq!(app.timelines[0].modified_count(), 1);
    }

    #[test]
    fn reused_source_id_with_new_creation_never_controls_or_navigates_focus() {
        let mut app = edited_timeline_app();
        app.focus.as_mut().unwrap().resource = Some(timeline_resource(11));
        assert!(app.input(key(' ')).is_empty());
        assert!(!app.control_pending);
        app.timeline_row = 1;
        assert!(app.input(key('N')).is_empty());
        assert_eq!(app.timeline_row, 1);
        assert!(app
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("open this timeline's source"));
    }

    #[test]
    fn editing_save_failure_survives_refresh_and_retry_persists_edits() {
        let directory = std::env::temp_dir().join(format!(
            "seedbox-model-timeline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&directory, b"block directory creation").unwrap();
        let mut app = edited_timeline_app();
        app.timeline_directory = Some(directory.clone());
        app.input(key('d'));
        assert!(app.timelines[0].is_unsaved());
        assert!(app.timelines[0].save_error().is_some());
        let entries = app.timelines[0].entries().to_vec();
        app.timeline_update(seedbox_api::FaultTimeline {
            simulation: "k7m2p9qx".parse().unwrap(),
            entries,
        });
        assert!(app.timelines[0].is_unsaved());
        assert_eq!(app.timelines[0].modified_count(), 1);
        std::fs::remove_file(&directory).unwrap();
        app.timelines[0].retry_save(&directory).unwrap();
        assert!(!app.timelines[0].is_unsaved());
        assert!(app.timelines[0].save_error().is_none());
        let (saved, _) = crate::timeline::TimelineDraft::load_all(&directory).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].modified_count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unknown_admitted_fault_is_persisted_before_immediate_quit() {
        let directory = std::env::temp_dir().join(format!(
            "seedbox-immediate-timeline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut app = app(&["a"]);
        app.focus.as_mut().unwrap().resource = Some(timeline_resource(10));
        app.timeline_directory = Some(directory.clone());
        app.apply(Update::FaultReceipt {
            title:"future_action".into(), request:serde_json::json!({"command":{"type":"injectFault","action":{"action":"future_action","newField":{"value":7}}}}).to_string(),
            result:Ok(serde_json::json!({"state":{"type":"acknowledged","error":null,
                "intervention":{"requestId":"new-request","position":{"boundaryIndex":4,"completedRounds":3,"simulatedTimeNs":100},
                "plan":{"id":1,"at_ns":100,"deadline_ns":200,"action":"future_action","newField":{"value":7}}}}})),
        });
        app.input(key('q'));
        assert!(app.quit);
        let (saved, _) = crate::timeline::TimelineDraft::load_all(&directory).unwrap();
        assert_eq!(saved[0].entries()[0].id, "program:new-request");
        assert_eq!(
            saved[0].entries()[0].action,
            serde_json::json!({"action":"future_action","newField":{"value":7}})
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn heal_catalogue() -> FaultCatalogue {
        FaultCatalogue {
            simulation: "k7m2p9qx".parse().unwrap(),
            faults: vec![seedbox_api::FaultDescriptor {
                name: "heal".into(),
                title: "Heal fault".into(),
                description: String::new(),
                schema: serde_json::json!({"type":"object","required":["action","handle"],"properties":{
                    "action":{"const":"heal"},"handle":{"type":"string","enum":["p1"]}}}),
            }],
        }
    }

    fn partitioned(app: &mut App) {
        let mut w = world(&["a", "b"]);
        w.revision = 2;
        w.faults.push(seedbox_api::FaultState {
            id: 1,
            input_key: None,
            action: "partition".into(),
            machine: None,
            status: seedbox_api::FaultStatus::Applied,
            applied_time_ns: Some(10),
            plan: Some(
                serde_json::json!({"action":"partition","handle":"p1","left":["a"],"right":["b"]}),
            ),
            active: true,
        });
        app.apply(Update::World(w));
    }

    #[test]
    fn heal_key_closes_the_active_handle_once_and_checks_the_catalogue() {
        let mut app = app(&["a", "b"]);
        partitioned(&mut app);
        assert!(app.input(key('h')).is_empty());
        assert!(app.notice.as_ref().unwrap().text.contains("offers no heal"));
        app.apply(Update::FaultTypes(heal_catalogue()));
        assert_eq!(
            app.input(key('h')),
            [Request::Heal(
                "k7m2p9qx".parse().unwrap(),
                serde_json::json!({"action":"heal","handle":"p1"})
            )]
        );
        // A second heal for the same handle is held back while the first is pending.
        assert!(app.input(key('h')).is_empty());
        assert!(app.notice.as_ref().unwrap().text.contains("heal already"));
    }

    #[test]
    fn run_to_next_fault_needs_an_upcoming_input() {
        let mut app = app(&["a"]);
        assert!(app.input(key('n')).is_empty());
        assert!(app
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("no upcoming fault"));
        app.focus.as_mut().unwrap().timeline = vec![seedbox_api::TimelineEntry {
            id: "program:later".into(),
            at_ns: 5_000_000_000,
            action: serde_json::json!({"action":"stop","machine":"a"}),
            origin: seedbox_api::TimelineOrigin::Program,
            position: None,
            fault_id: None,
        }];
        assert!(matches!(
            app.input(key('n')).as_slice(),
            [Request::Control(
                _,
                ControlCommand::PauseBeforeNextFault { .. }
            )]
        ));
    }

    #[test]
    fn tab_moves_focus_and_arrows_follow_it() {
        let mut app = app(&["a", "b"]);
        partitioned(&mut app);
        let tab = || Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.panel, Panel::Machines);
        app.input(key('j'));
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("b"));
        app.input(tab());
        assert_eq!(app.panel, Panel::Faults);
        app.input(key('j'));
        assert_eq!(
            app.focus.as_ref().unwrap().fault_selected,
            Some(Key::Fault(1))
        );
        // Machine selection is untouched while faults have focus.
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("b"));
        app.input(tab());
        assert_eq!(app.panel, Panel::Logs);
        app.input(key('k'));
        assert_eq!(app.logs_scroll, 1);
    }

    #[test]
    fn catalogue_nests_derived_runs_and_tolerates_cycles() {
        let summary = |id: &str, created, source: Option<&str>| seedbox_api::SimulationSummary {
            metadata: seedbox_api::Metadata {
                name: "demo".into(),
                labels: Default::default(),
                annotations: source
                    .map(|s| [("derived-from".to_string(), s.to_string())].into())
                    .unwrap_or_default(),
                id: id.parse().unwrap(),
                revision: 1,
                created_at_unix_ms: created,
                deletion_requested_at_unix_ms: None,
                parent: None,
                checkpoint: None,
            },
            status: Default::default(),
            oracle: "oracle".into(),
        };
        let mut app = App::new(None);
        app.apply(Update::List(vec![
            summary("aaaaaaaa", 1, None),
            summary("bbbbbbbb", 2, Some("aaaaaaaa")),
            summary("cccccccc", 3, Some("bbbbbbbb")),
            summary("dddddddd", 4, Some("eeeeeeee")),
            summary("eeeeeeee", 5, Some("dddddddd")),
        ]));
        let tree: Vec<(usize, String)> = app
            .tree()
            .iter()
            .map(|(depth, s)| (*depth, s.metadata.id.to_string()))
            .collect();
        assert!(tree.contains(&(0, "aaaaaaaa".into())));
        assert!(tree.contains(&(1, "bbbbbbbb".into())));
        assert!(tree.contains(&(2, "cccccccc".into())));
        // A cycle is listed once per member rather than looping.
        assert_eq!(tree.len(), 5);
    }

    #[test]
    fn traffic_rates_follow_simulated_time_and_survive_counter_resets() {
        let mut traffic = Traffic::default();
        let mut w = world(&["a", "b"]);
        traffic.observe(&w);
        w.simulated_time_ns = 1_000_000_000;
        for link in &mut w.links {
            if link.from == "a" {
                link.counters.scheduled = 500;
            }
        }
        traffic.observe(&w);
        assert_eq!(traffic.sent_rate("a"), 500.0);
        assert_eq!(traffic.received_rate("b"), 500.0);
        assert_eq!(traffic.link_rate("a", "b"), 500.0);
        // A restored run starts counting again; rates never go negative.
        w.simulated_time_ns = 2_000_000_000;
        for link in &mut w.links {
            link.counters.scheduled = 0;
        }
        traffic.observe(&w);
        assert_eq!(traffic.sent_rate("a"), 0.0);
    }

    #[test]
    fn machines_keep_their_slot_when_others_come_and_go() {
        let mut app = app(&["a", "b", "c"]);
        let mut fewer = world(&["a", "c"]);
        fewer.revision = 2;
        app.apply(Update::World(fewer));
        let mut more = world(&["a", "c", "d"]);
        more.revision = 3;
        app.apply(Update::World(more));
        assert_eq!(app.focus.as_ref().unwrap().seen, ["a", "b", "c", "d"]);
    }

    #[test]
    fn keys_typed_while_the_palette_opens_are_replayed_into_it() {
        let mut app = app(&["a"]);
        app.apply(Update::FaultTypes(fault_catalogue("stop")));
        assert_eq!(app.input(key(':')).len(), 1);
        for c in "zzz".chars() {
            assert!(app.input(key(c)).is_empty());
        }
        app.apply(Update::FaultMenuReady(Ok(PlaybackRate::Unlimited)));
        // The filter matches nothing, so Enter cannot pick an entry by accident.
        assert!(app
            .input(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            )))
            .is_empty());
        assert!(app.fault_form.is_some());
    }

    #[test]
    fn clicking_off_a_machine_shows_every_machines_logs() {
        let mut app = app(&["a", "b"]);
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("a"));
        app.hitboxes.borrow_mut().push((
            ratatui::layout::Rect::new(0, 0, 10, 1),
            HitTarget::Machine(1),
        ));
        let click = |column, row| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.input(click(2, 0));
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("b"));
        app.input(click(40, 20));
        assert_eq!(app.focus.as_ref().unwrap().selected, None);
    }
}
