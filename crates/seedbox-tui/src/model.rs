//! Client-side state. Every machine, link and fault shown comes from the API;
//! nothing here knows a world's shape in advance.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use seedbox_api::{
    ControlCommand, ControlReceipt, EventDelivery, EventKind, FaultAction, Frame, FrameDelivery,
    FrameFate, LogDelivery, LogEntry, LogStream, MachineCondition, PlaybackObservation,
    PlaybackRate, Simulation, SimulationComparison, SimulationEvent, SimulationId,
    SimulationSummary, WorldState,
};
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

const MAX_EVENTS: usize = 500;
const MAX_LOG_LINES: usize = 1000;
const MAX_FRAMES: usize = 4000;
const NOTICE_FOR: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Catalogue,
    Simulation,
}

/// Everything the API tasks report back to the UI.
#[derive(Debug)]
pub enum Update {
    List(Vec<SimulationSummary>),
    Resource(Simulation),
    World(WorldState),
    Playback(PlaybackObservation),
    Event(EventDelivery),
    Log(LogDelivery),
    Frames(FrameDelivery),
    Receipt {
        title: String,
        request: String,
        result: Result<ControlReceipt, String>,
    },
    Checkpoints(Vec<String>),
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
    Control(SimulationId, ControlCommand),
    Rerun(SimulationId),
    Branch(SimulationId),
    Stop(SimulationId),
    Delete(SimulationId),
    Compare(SimulationId, SimulationId),
    RefreshWorld(SimulationId),
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
            self.lines.push_back(LogLine {
                time_ns,
                stream,
                text,
            });
            if self.lines.len() > MAX_LOG_LINES {
                self.lines.pop_front();
            }
        }
        if self.partial.len() > 4096 {
            let text = String::from_utf8_lossy(&self.partial).to_string();
            self.partial.clear();
            self.lines.push_back(LogLine {
                time_ns,
                stream,
                text,
            });
        }
    }

    fn note(&mut self, time_ns: u64, text: String) {
        self.lines.push_back(LogLine {
            time_ns,
            stream: LogStream::Stderr,
            text,
        });
    }
}

/// One simulation opened for inspection.
pub struct Focus {
    pub id: SimulationId,
    pub resource: Option<Simulation>,
    pub world: Option<WorldState>,
    pub playback: Option<(PlaybackObservation, Instant)>,
    pub frames: VecDeque<Frame>,
    pub frame_time_ns: u64,
    pub events: VecDeque<SimulationEvent>,
    pub logs: BTreeMap<String, MachineLog>,
    pub selected: Option<String>,
    pub checkpoints: Vec<String>,
    /// The rate to resume at after a pause.
    pub resume_rate: PlaybackRate,
    pub lost: u64,
}

impl Focus {
    fn new(id: SimulationId) -> Self {
        Self {
            id,
            resource: None,
            world: None,
            playback: None,
            frames: VecDeque::new(),
            frame_time_ns: 0,
            events: VecDeque::new(),
            logs: BTreeMap::new(),
            selected: None,
            checkpoints: Vec::new(),
            resume_rate: PlaybackRate::Unlimited,
            lost: 0,
        }
    }

    /// Machine names in display order, straight from the observed world.
    pub fn machines(&self) -> Vec<String> {
        self.world
            .as_ref()
            .map(|w| w.machines.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn knows(&self, machine: &str) -> bool {
        self.world
            .as_ref()
            .is_some_and(|w| w.machines.iter().any(|m| m.name == machine))
    }

    pub fn paused(&self) -> bool {
        matches!(
            self.playback.as_ref().map(|(p, _)| p.state.rate),
            Some(PlaybackRate::Paused)
        )
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

    /// Best estimate of the simulation's current time: the last committed
    /// position, carried forward at a limited rate between samples.
    pub fn now_ns(&self, wall: Instant) -> u64 {
        let mut now = self.frame_time_ns;
        if let Some(world) = &self.world {
            now = now.max(world.simulated_time_ns);
        }
        if let Some(resource) = &self.resource {
            now = now.max(resource.status.simulated_time_ns);
        }
        if let Some((sample, at)) = &self.playback {
            let base = sample.state.position.simulated_time_ns;
            let carried = match sample.state.rate {
                PlaybackRate::Limited {
                    numerator,
                    denominator,
                } if !sample.state.terminal && !sample.state.stepping => {
                    let wall_ns = wall
                        .saturating_duration_since(*at)
                        .min(Duration::from_secs(1));
                    (wall_ns.as_nanos() as u64).saturating_mul(u64::from(numerator.get()))
                        / u64::from(denominator.get())
                }
                _ => 0,
            };
            now = now.max(base.saturating_add(carried));
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

pub struct App {
    pub screen: Screen,
    pub list: Vec<SimulationSummary>,
    pub list_selected: usize,
    pub focus: Option<Focus>,
    pub notice: Option<Notice>,
    pub inspector: Option<Inspector>,
    pub palette: Option<String>,
    pub comparison: Option<SimulationComparison>,
    pub baseline: Option<SimulationId>,
    pub confirm_delete: Option<SimulationId>,
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
    pub fn new(focus: Option<SimulationId>) -> Self {
        Self {
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
            palette: None,
            comparison: None,
            baseline: None,
            confirm_delete: None,
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

    /// Catalogue rows in tree order: each simulation followed by its children.
    pub fn tree(&self) -> Vec<(usize, &SimulationSummary)> {
        let mut out = Vec::new();
        let ids: std::collections::BTreeSet<&SimulationId> =
            self.list.iter().map(|s| &s.metadata.id).collect();
        fn visit<'a>(
            list: &'a [SimulationSummary],
            parent: Option<&SimulationId>,
            depth: usize,
            out: &mut Vec<(usize, &'a SimulationSummary)>,
        ) {
            for s in list.iter().filter(|s| s.metadata.parent.as_ref() == parent) {
                out.push((depth, s));
                if depth < 8 {
                    visit(list, Some(&s.metadata.id), depth + 1, out);
                }
            }
        }
        visit(&self.list, None, 0, &mut out);
        // Children whose parent is not listed (deleted, or on a later page).
        for s in &self.list {
            if s.metadata.parent.as_ref().is_some_and(|p| !ids.contains(p)) {
                out.push((1, s));
                visit(&self.list, Some(&s.metadata.id), 2, &mut out);
            }
        }
        out
    }

    fn selected_summary(&self) -> Option<SimulationId> {
        self.tree()
            .get(self.list_selected)
            .map(|(_, s)| s.metadata.id.clone())
    }

    pub fn apply(&mut self, update: Update) -> Vec<Request> {
        let mut requests = Vec::new();
        match update {
            Update::List(list) => {
                self.list = list;
                self.list_selected = self.list_selected.min(self.list.len().saturating_sub(1));
            }
            Update::Resource(resource) => {
                if let Some(focus) = self.focus_for(&resource.metadata.id) {
                    focus.resource = Some(resource);
                }
            }
            Update::World(world) => {
                if let Some(focus) = self.focus_for(&world.simulation) {
                    let names: Vec<&String> = world.machines.iter().map(|m| &m.name).collect();
                    if !focus.selected.as_ref().is_some_and(|s| names.contains(&s)) {
                        focus.selected = world
                            .machines
                            .iter()
                            .find(|m| !m.oracle)
                            .or(world.machines.first())
                            .map(|m| m.name.clone());
                    }
                    focus.world = Some(world);
                }
            }
            Update::Playback(sample) => {
                if let Some(focus) = self.focus.as_mut() {
                    if !matches!(sample.state.rate, PlaybackRate::Paused) {
                        focus.resume_rate = sample.state.rate;
                    }
                    focus.playback = Some((sample, Instant::now()));
                }
            }
            Update::Event(delivery) => {
                if let Some(focus) = self.focus.as_mut() {
                    match delivery {
                        EventDelivery::Event { observation, .. } => {
                            let refresh = matches!(
                                &observation.event,
                                EventKind::FaultChanged { .. }
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
                        EventDelivery::Gap { first, last } => focus.lost += last - first + 1,
                        EventDelivery::Closed => {}
                    }
                }
            }
            Update::Log(delivery) => {
                if let Some(focus) = self.focus.as_mut() {
                    if let LogDelivery::Entry { entry, .. } = delivery {
                        match entry {
                            LogEntry::Bytes {
                                machine,
                                simulated_time_ns,
                                stream,
                                bytes,
                                ..
                            } => {
                                if !focus.knows(&machine) {
                                    requests.push(Request::RefreshWorld(focus.id.clone()));
                                }
                                focus.logs.entry(machine).or_default().push(
                                    simulated_time_ns,
                                    stream,
                                    &bytes,
                                );
                            }
                            LogEntry::MachineGap {
                                machine,
                                simulated_time_ns,
                                records,
                                ..
                            } => focus.logs.entry(machine).or_default().note(
                                simulated_time_ns,
                                format!("[{records} output records were lost]"),
                            ),
                            _ => {}
                        }
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
                            if frames
                                .iter()
                                .any(|f| !focus.knows(&f.from) || !focus.knows(&f.to))
                            {
                                requests.push(Request::RefreshWorld(focus.id.clone()));
                            }
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
            Update::Checkpoints(names) => {
                if let Some(focus) = self.focus.as_mut() {
                    focus.checkpoints = names;
                }
            }
            Update::Comparison(comparison) => self.comparison = Some(comparison),
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
                    Ok(simulation) if self.screen == Screen::Simulation => {
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

    pub fn input(&mut self, event: Event) -> Vec<Request> {
        let Event::Key(key) = event else {
            return Vec::new();
        };
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Vec::new();
        }
        if self.palette.is_some() {
            return self.palette_key(key);
        }
        if self.help {
            self.help = false;
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return Vec::new();
            }
            KeyCode::Char('?') => {
                self.help = true;
                return Vec::new();
            }
            KeyCode::Char('1') => {
                self.screen = Screen::Catalogue;
                return Vec::new();
            }
            KeyCode::Char('2') if self.focus.is_some() => {
                self.screen = Screen::Simulation;
                return Vec::new();
            }
            _ => {}
        }
        match self.screen {
            Screen::Catalogue => self.catalogue_key(key),
            Screen::Simulation => self.simulation_key(key),
        }
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
            KeyCode::Char('r') => return selected.map(Request::Rerun).into_iter().collect(),
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
                    return vec![Request::Compare(baseline, id)]
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
        self.focus = Some(Focus::new(id.clone()));
        self.inspector = None;
        vec![Request::Open(id)]
    }

    fn simulation_key(&mut self, key: KeyEvent) -> Vec<Request> {
        let Some(focus) = self.focus.as_mut() else {
            return Vec::new();
        };
        let id = focus.id.clone();
        let control = |command| vec![Request::Control(id.clone(), command)];
        match key.code {
            KeyCode::Esc => self.screen = Screen::Catalogue,
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => select(focus, 1),
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => select(focus, -1),
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
                    let mut out = control(ControlCommand::SetRate {
                        rate: PlaybackRate::Paused,
                    });
                    out.push(Request::Control(id.clone(), ControlCommand::Step));
                    return out;
                }
                return control(ControlCommand::Step);
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
            KeyCode::Char('p') => {
                if let Some(action) = partition_selected(focus) {
                    return control(ControlCommand::InjectFault { action });
                }
            }
            KeyCode::Char('s') => {
                if let Some(action) = shape_selected(focus) {
                    return control(ControlCommand::InjectFault { action });
                }
            }
            KeyCode::Char('x') => {
                if let Some(action) = crash_or_reboot(focus) {
                    return control(ControlCommand::InjectFault { action });
                }
            }
            KeyCode::Char('h') => {
                let handles = active_handles(focus);
                if handles.is_empty() {
                    self.notify("no active partitions or shapes", Tone::Info);
                }
                return handles
                    .into_iter()
                    .map(|handle| {
                        Request::Control(
                            id.clone(),
                            ControlCommand::InjectFault {
                                action: FaultAction::Heal { handle },
                            },
                        )
                    })
                    .collect();
            }
            KeyCode::Char('c') => return control(ControlCommand::Checkpoint),
            KeyCode::Char('b') => return vec![Request::Branch(id.clone())],
            KeyCode::Char('r') => return vec![Request::Rerun(id.clone())],
            KeyCode::Char(':') => self.palette = Some(String::new()),
            _ => {}
        }
        Vec::new()
    }

    fn palette_key(&mut self, key: KeyEvent) -> Vec<Request> {
        let Some(text) = self.palette.as_mut() else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            KeyCode::Enter => {
                let text = self.palette.take().unwrap_or_default();
                let Some(focus) = self.focus.as_ref() else {
                    return Vec::new();
                };
                match parse_action(&text, &focus.machines()) {
                    Ok(action) => {
                        return vec![Request::Control(
                            focus.id.clone(),
                            ControlCommand::InjectFault { action },
                        )]
                    }
                    Err(error) => self.notify(error, Tone::Bad),
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

fn select(focus: &mut Focus, step: isize) {
    let machines = focus.machines();
    if machines.is_empty() {
        return;
    }
    let current = focus
        .selected
        .as_ref()
        .and_then(|s| machines.iter().position(|m| m == s))
        .unwrap_or(0) as isize;
    let next = (current + step).rem_euclid(machines.len() as isize) as usize;
    focus.selected = Some(machines[next].clone());
}

fn others(focus: &Focus, machine: &str) -> Vec<String> {
    focus
        .machines()
        .into_iter()
        .filter(|m| m != machine)
        .collect()
}

fn unique_handle(focus: &Focus, prefix: &str) -> String {
    let used: Vec<String> = focus
        .world
        .iter()
        .flat_map(|w| &w.faults)
        .filter_map(|f| match &f.plan {
            Some(FaultAction::Partition { handle, .. } | FaultAction::Shape { handle, .. }) => {
                Some(handle.clone())
            }
            _ => None,
        })
        .collect();
    (1..)
        .map(|n| format!("{prefix}{n}"))
        .find(|h| !used.contains(h))
        .expect("an unused handle")
}

fn active_with(focus: &Focus, test: impl Fn(&FaultAction) -> bool) -> Option<String> {
    focus.world.as_ref()?.faults.iter().find_map(|f| {
        f.plan
            .as_ref()
            .filter(|plan| f.active && test(plan))
            .and_then(|plan| match plan {
                FaultAction::Partition { handle, .. } | FaultAction::Shape { handle, .. } => {
                    Some(handle.clone())
                }
                _ => None,
            })
    })
}

/// Partition the selected machine from every other machine, or heal the
/// partition that already isolates it.
pub fn partition_selected(focus: &Focus) -> Option<FaultAction> {
    let machine = focus.selected.clone()?;
    if let Some(handle) = active_with(
        focus,
        |plan| matches!(plan, FaultAction::Partition { left, .. } if left == std::slice::from_ref(&machine)),
    ) {
        return Some(FaultAction::Heal { handle });
    }
    let right = others(focus, &machine);
    if right.is_empty() {
        return None;
    }
    Some(FaultAction::Partition {
        handle: unique_handle(focus, "tui-partition-"),
        left: vec![machine],
        right,
    })
}

/// Slow and lossy outbound links from the selected machine, or heal them.
pub fn shape_selected(focus: &Focus) -> Option<FaultAction> {
    let machine = focus.selected.clone()?;
    if let Some(handle) = active_with(
        focus,
        |plan| matches!(plan, FaultAction::Shape { from, .. } if from == std::slice::from_ref(&machine)),
    ) {
        return Some(FaultAction::Heal { handle });
    }
    let to = others(focus, &machine);
    if to.is_empty() {
        return None;
    }
    let base = focus.link_latency(&machine, &to[0]);
    Some(FaultAction::Shape {
        handle: unique_handle(focus, "tui-shape-"),
        from: vec![machine],
        to,
        latency_ns: base.saturating_mul(4),
        jitter_ns: base,
        loss_ppm: 30_000,
        bandwidth_bps: None,
        queue_bound_ns: None,
        network_faults: Default::default(),
    })
}

pub fn crash_or_reboot(focus: &Focus) -> Option<FaultAction> {
    let machine = focus.selected.clone()?;
    let state = focus
        .world
        .as_ref()?
        .machines
        .iter()
        .find(|m| m.name == machine)?;
    match state.condition {
        MachineCondition::Crashed => Some(FaultAction::Reboot { machine }),
        MachineCondition::Exited => None,
        _ => Some(FaultAction::Crash { machine }),
    }
}

pub fn active_handles(focus: &Focus) -> Vec<String> {
    focus
        .world
        .iter()
        .flat_map(|w| &w.faults)
        .filter(|f| f.active)
        .filter_map(|f| match &f.plan {
            Some(FaultAction::Partition { handle, .. } | FaultAction::Shape { handle, .. }) => {
                Some(handle.clone())
            }
            _ => None,
        })
        .collect()
}

/// A palette line is a YAML or JSON fault action. Machine names are checked
/// against the observed world before anything is sent.
pub fn parse_action(text: &str, machines: &[String]) -> Result<FaultAction, String> {
    let action: FaultAction =
        serde_yaml_ng::from_str(text.trim()).map_err(|e| format!("fault action: {e}"))?;
    let value = serde_json::to_value(&action).map_err(|e| e.to_string())?;
    let mut named = Vec::new();
    for key in ["machine", "left", "right", "from", "to"] {
        match value.get(key) {
            Some(serde_json::Value::String(name)) => named.push(name.clone()),
            Some(serde_json::Value::Array(names)) => {
                named.extend(names.iter().filter_map(|n| n.as_str().map(str::to_owned)))
            }
            _ => {}
        }
    }
    if let Some(unknown) = named.iter().find(|n| !machines.contains(n)) {
        return Err(format!("unknown machine {unknown}"));
    }
    Ok(action)
}

pub fn rate_label(rate: PlaybackRate) -> String {
    match rate {
        PlaybackRate::Paused => "paused".into(),
        PlaybackRate::Unlimited => "unlimited".into(),
        PlaybackRate::Limited {
            numerator,
            denominator,
        } => format!("{numerator}/{denominator}×"),
    }
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
    use seedbox_api::{FaultState, FaultStatus, LinkCounters, LinkPolicy, LinkState, MachineState};

    pub(crate) fn world(names: &[&str]) -> WorldState {
        let machines = names
            .iter()
            .map(|n| MachineState {
                name: (*n).into(),
                incarnation: 0,
                condition: MachineCondition::Ready,
                exit_code: None,
                throttle_permille: None,
                oracle: *n == "oracle",
                last_transition: None,
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
        app
    }

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn machines_come_and_go_with_the_world() {
        let mut app = app(&["a", "b", "oracle"]);
        assert_eq!(app.focus.as_ref().unwrap().selected.as_deref(), Some("a"));
        app.apply(Update::World(world(&["b", "late", "oracle"])));
        let focus = app.focus.as_ref().unwrap();
        assert_eq!(focus.machines(), ["b", "late", "oracle"]);
        assert_eq!(focus.selected.as_deref(), Some("b"));
    }

    #[test]
    fn unknown_machines_in_streams_trigger_a_world_refresh() {
        let mut app = app(&["a"]);
        let requests = app.apply(Update::Log(LogDelivery::Entry {
            sequence: 1,
            entry: LogEntry::Bytes {
                machine: "new".into(),
                incarnation: 0,
                simulated_time_ns: 5,
                stream: LogStream::Stdout,
                bytes: b"hello\nwor".to_vec(),
            },
        }));
        assert_eq!(
            requests,
            [Request::RefreshWorld("k7m2p9qx".parse().unwrap())]
        );
        let log = &app.focus.as_ref().unwrap().logs["new"];
        assert_eq!(log.lines.len(), 1);
        assert_eq!(log.lines[0].text, "hello");
    }

    #[test]
    fn partition_targets_every_other_observed_machine_then_heals() {
        let mut app = app(&["a", "b", "c"]);
        let requests = app.input(key('p'));
        let Request::Control(_, ControlCommand::InjectFault { action }) = &requests[0] else {
            panic!("{requests:?}");
        };
        assert_eq!(
            action,
            &FaultAction::Partition {
                handle: "tui-partition-1".into(),
                left: vec!["a".into()],
                right: vec!["b".into(), "c".into()],
            }
        );
        let mut w = world(&["a", "b", "c"]);
        w.faults.push(FaultState {
            id: 4,
            action: "partition".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(1),
            plan: Some(action.clone()),
            active: true,
        });
        app.apply(Update::World(w));
        let requests = app.input(key('p'));
        assert_eq!(
            requests,
            [Request::Control(
                "k7m2p9qx".parse().unwrap(),
                ControlCommand::InjectFault {
                    action: FaultAction::Heal {
                        handle: "tui-partition-1".into()
                    }
                }
            )]
        );
    }

    #[test]
    fn step_pauses_a_running_simulation_first() {
        let mut app = app(&["a"]);
        let requests = app.input(key('.'));
        assert!(matches!(
            requests.as_slice(),
            [
                Request::Control(
                    _,
                    ControlCommand::SetRate {
                        rate: PlaybackRate::Paused
                    }
                ),
                Request::Control(_, ControlCommand::Step)
            ]
        ));
    }

    #[test]
    fn palette_rejects_machines_the_world_does_not_have() {
        let machines = vec!["a".to_string(), "b".to_string()];
        assert!(parse_action("{action: crash, machine: a}", &machines).is_ok());
        assert_eq!(
            parse_action(
                "{action: partition, handle: x, left: [a], right: [zz]}",
                &machines
            )
            .unwrap_err(),
            "unknown machine zz"
        );
        assert!(parse_action("{action: nonsense}", &machines).is_err());
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
}
