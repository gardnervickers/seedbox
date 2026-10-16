//! Rendering. Layout is computed from whatever the world reports each frame
//! and adapts to the terminal: wide terminals show every panel, narrow ones
//! fold the side panels into tabs and draw machines as badges.
use crate::activity::{Activity, Item, Lane, State};
use crate::model::{rate_label, simulation_name, App, Focus, HitTarget, Panel, Screen, Tone};
use palette::*;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Painter, Shape},
        Block, BorderType, Clear, Paragraph, Wrap,
    },
    Frame,
};
use seedbox_api::{
    EventKind, FaultStatus, FrameFate, LogStream, MachineCondition, MachineState, Phase,
    PlaybackRate, SimulationSummary, TestVerdict,
};
use std::time::Instant;

pub mod palette {
    use ratatui::style::Color;
    pub const ICE: Color = Color::Rgb(120, 200, 255);
    pub const ICE_BRIGHT: Color = Color::Rgb(181, 225, 255);
    pub const ICE_MID: Color = Color::Rgb(72, 136, 186);
    pub const ICE_DIM: Color = Color::Rgb(51, 85, 111);
    pub const TEAL: Color = Color::Rgb(79, 224, 194);
    pub const AMBER: Color = Color::Rgb(255, 181, 71);
    pub const AMBER_DEEP: Color = Color::Rgb(138, 101, 40);
    pub const RED: Color = Color::Rgb(255, 90, 106);
    pub const RED_SOFT: Color = Color::Rgb(224, 124, 136);
    pub const RED_DEEP: Color = Color::Rgb(142, 43, 57);
    pub const YELLOW: Color = Color::Rgb(233, 207, 91);
    pub const GREEN: Color = Color::Rgb(89, 211, 140);
    pub const ORANGE: Color = Color::Rgb(255, 143, 74);
    pub const MAGENTA: Color = Color::Rgb(212, 140, 255);
    pub const WHITE: Color = Color::Rgb(242, 246, 250);
    pub const TEXT: Color = Color::Rgb(200, 211, 223);
    pub const DIM: Color = Color::Rgb(131, 148, 170);
    pub const FAINT_TEXT: Color = Color::Rgb(93, 112, 137);
    pub const FAINT: Color = Color::Rgb(42, 57, 75);
    pub const BORDER: Color = Color::Rgb(30, 44, 60);
    pub const BORDER_HI: Color = Color::Rgb(72, 100, 134);
    pub const BAR: Color = Color::Rgb(14, 22, 34);
    pub const SELECTED: Color = Color::Rgb(19, 36, 58);
    pub const NODE: Color = Color::Rgb(11, 17, 25);
    pub const BANNER: Color = Color::Rgb(42, 30, 10);

    /// Colour of a fault span. Handles and one-shot disruptions read red,
    /// recoveries green, shaping and clock changes yellow.
    pub fn span_color(action: &str, strong: bool) -> Color {
        let strong_color = match action {
            "heal" | "reboot" | "restart" | "resume" | "resume_process" | "resume_application" => {
                GREEN
            }
            "shape" | "throttle" | "clock_skew" | "clock_drift" | "skew" | "drift" => YELLOW,
            "stop" | "pause" | "pause_application" | "stop_application" => AMBER,
            _ => RED,
        };
        if strong {
            return strong_color;
        }
        match strong_color {
            GREEN => Color::Rgb(46, 110, 74),
            YELLOW => Color::Rgb(120, 106, 46),
            AMBER => AMBER_DEEP,
            _ => RED_DEEP,
        }
    }

    /// A stable colour per machine name, so it reads the same everywhere and
    /// survives machines coming and going.
    pub fn machine_color(name: &str) -> Color {
        const COLORS: [Color; 6] = [
            ICE,
            TEAL,
            Color::Rgb(186, 160, 255),
            Color::Rgb(255, 170, 120),
            Color::Rgb(140, 210, 120),
            Color::Rgb(250, 140, 190),
        ];
        let hash = name.bytes().fold(2166136261u32, |h, b| {
            (h ^ u32::from(b)).wrapping_mul(16777619)
        });
        COLORS[hash as usize % COLORS.len()]
    }
}

/// Simulated nanoseconds as seconds with a thin group separator.
pub fn time(ns: u64) -> String {
    let s = format!("{:.6}", ns as f64 / 1e9);
    let (head, tail) = s.split_at(s.len() - 3);
    format!("{head} {tail} s")
}

/// Compact simulated time: up to millisecond precision, trailing zeros trimmed.
pub fn short_time(ns: u64) -> String {
    let text = format!("{:.3}", ns as f64 / 1e9);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}s")
}

/// Time until a scheduled item, or "due now" when it is due.
fn until(at: u64, now: u64) -> String {
    if at <= now {
        "due now".into()
    } else {
        format!("in {}", duration(at - now))
    }
}

/// A human duration for spans and countdowns.
pub fn duration(ns: u64) -> String {
    let seconds = ns as f64 / 1e9;
    if ns < 1_000_000_000 {
        format!("{}ms", ns / 1_000_000)
    } else if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else if seconds < 3600.0 {
        format!("{}m{:02}s", (seconds / 60.0) as u64, seconds as u64 % 60)
    } else {
        format!(
            "{}h{:02}m",
            (seconds / 3600.0) as u64,
            (seconds as u64 / 60) % 60
        )
    }
}

/// A short elapsed time for tight spaces: seconds, then whole minutes or hours.
fn count(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=9_999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Frames per simulated second, compact.
fn rate(per_second: f64) -> String {
    match per_second {
        r if r < 0.5 => "0".into(),
        r if r < 1000.0 => format!("{r:.0}"),
        r if r < 10_000.0 => format!("{:.1}k", r / 1e3),
        r if r < 1_000_000.0 => format!("{:.0}k", r / 1e3),
        r => format!("{:.1}M", r / 1e6),
    }
}

/// A link delay in milliseconds, or microseconds below one.
pub fn latency(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{}ms", ns as f64 / 1e6)
    } else {
        format!("{}µs", ns as f64 / 1e3)
    }
}

/// Recent transmit rates as bars, scaled to the busiest machine so machines
/// compare at a glance and steady traffic does not fill the box.
fn sparkline(focus: &Focus, machine: &str, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let Some(history) = focus.traffic.sent.get(machine) else {
        return "▁".repeat(width);
    };
    let peak = focus
        .traffic
        .sent
        .values()
        .flat_map(|h| h.iter().copied())
        .fold(1.0f64, f64::max)
        * 1.25;
    let mut out: String = history
        .iter()
        .rev()
        .take(width)
        .map(|value| BARS[((value / peak) * 7.0).round().clamp(0.0, 7.0) as usize])
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    while out.chars().count() < width {
        out.insert(0, ' ');
    }
    out
}

/// Recent achieved rates as bars, scaled to the fastest in view.
fn rate_history(rates: &std::collections::VecDeque<f64>, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let recent: Vec<f64> = rates.iter().rev().take(width).rev().copied().collect();
    let peak = recent.iter().copied().fold(f64::MIN_POSITIVE, f64::max);
    recent
        .iter()
        .map(|value| BARS[((value / peak) * 7.0).round().clamp(0.0, 7.0) as usize])
        .collect()
}

/// A glyph for a fault action. Unknown actions get a neutral mark.
pub fn glyph(action: &str, state: State) -> char {
    if state == State::Upcoming {
        return '◇';
    }
    if state == State::Failed {
        return '✕';
    }
    match action {
        "partition" => '✂',
        "shape" => '≈',
        "heal" => '✚',
        "crash" | "crashed" => '✕',
        "reboot" | "restart" => '⟲',
        "stop" | "pause" | "paused" => '◼',
        "resume" => '▶',
        "throttle" => '▼',
        "clock_skew" | "clock_drift" | "skew" | "drift" => '◷',
        action if action.starts_with("exited") => '✕',
        _ => '•',
    }
}

fn block(title: &str, focus: bool) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focus { BORDER_HI } else { BORDER }))
        .title(Span::styled(
            format!(" {title} "),
            Style::new()
                .fg(if focus { ICE_BRIGHT } else { ICE })
                .add_modifier(Modifier::BOLD),
        ))
}

fn source(text: &str, details: bool) -> Line<'_> {
    if !details {
        return Line::default();
    }
    Line::from(Span::styled(
        format!(" ⇄ {text} "),
        Style::new().fg(ICE_DIM),
    ))
    .right_aligned()
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn pad(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    let used = out.chars().count();
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

fn pad_left(text: &str, width: usize) -> String {
    let used = text.chars().count();
    if used >= width {
        return text.chars().take(width).collect();
    }
    format!("{}{text}", " ".repeat(width - used))
}

pub fn draw(frame: &mut Frame, app: &App) {
    let now = Instant::now();
    app.hitboxes.borrow_mut().clear();
    app.map_bottom.set(None);
    let area = frame.area();
    let banner = u16::from(!app.connected);
    let [top, banner_area, body, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(area);
    header(frame, app, top);
    if banner > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ◌ lost the local server ", Style::new().fg(AMBER).bold()),
                Span::styled(
                    "· retrying every second · showing the last state seen · start it with `seedbox serve`",
                    Style::new().fg(Color::Rgb(255, 208, 138)),
                ),
            ]))
            .style(Style::new().bg(BANNER)),
            banner_area,
        );
    }
    match (app.screen, app.focus.as_ref()) {
        (Screen::Simulation, Some(focus)) => simulation(frame, app, focus, body, now),
        (Screen::Timeline, _) => {
            if let Some(draft) = app.timelines.get(app.timeline_source) {
                let path = app
                    .timeline_directory
                    .as_ref()
                    .map(|dir| draft.path(dir))
                    .unwrap_or_default();
                let world = app
                    .focus
                    .as_ref()
                    .filter(|focus| {
                        &focus.id == draft.source()
                            && focus.resource.as_ref().is_some_and(|resource| {
                                resource.metadata.created_at_unix_ms == draft.created_at_unix_ms()
                            })
                    })
                    .and_then(|focus| focus.world.as_ref());
                crate::timeline_view::draw(
                    frame,
                    body,
                    draft,
                    app.timeline_row,
                    &path,
                    world,
                    (app.details, &app.hitboxes),
                );
            } else {
                frame.render_widget(
                    Paragraph::new(
                        "No saved fault timelines yet. Open a simulation and inject a fault with :",
                    )
                    .fg(DIM)
                    .block(block("fault timelines", true)),
                    body,
                );
            }
        }
        _ => catalogue(frame, app, body),
    }
    footer(frame, app, keys, now);
    if let Some(form) = &app.fault_form {
        form.draw(frame, area, app.map_bottom.get());
    }
    if app.help {
        help(frame, app, area);
    }
}

fn header(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Block::new().style(Style::new().bg(BAR)), area);
    let tab = |label: &str, on: bool| {
        Span::styled(
            format!(" {label} "),
            if on {
                Style::new()
                    .fg(ICE_BRIGHT)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(DIM)
            },
        )
    };
    let mut spans = vec![
        Span::styled(" ◢◤ SEEDBOX ", Style::new().fg(ICE).bold()),
        Span::raw(" "),
    ];
    let mut tabs = vec![(Screen::Catalogue, "1 catalogue")];
    if app.focus.is_some() {
        tabs.push((Screen::Simulation, "2 simulation"));
    }
    tabs.push((Screen::Timeline, "3 timeline"));
    let mut x = area.x + width_of(&spans) as u16;
    for (screen, label) in tabs {
        let span = tab(label, app.screen == screen);
        let width = span.width() as u16;
        app.hitboxes
            .borrow_mut()
            .push((Rect::new(x, area.y, width, 1), HitTarget::Screen(screen)));
        x += width;
        spans.push(span);
    }
    let left = width_of(&spans);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // The right side drops lower-priority parts until it fits.
    let connection = if app.connected {
        Span::styled("● connected ", Style::new().fg(GREEN))
    } else {
        Span::styled("◌ server unreachable ", Style::new().fg(AMBER).bold())
    };
    let mut parts: Vec<(u8, Vec<Span>)> = vec![(0, vec![connection])];
    if let Some(focus) = &app.focus {
        let (label, color) = phase(focus);
        parts.push((
            1,
            vec![Span::styled(
                format!("{label}  "),
                Style::new().fg(color).bold(),
            )],
        ));
        if let Some(resource) = &focus.resource {
            parts.push((
                2,
                vec![Span::styled(
                    format!("{}  ", simulation_name(resource)),
                    Style::new().fg(WHITE).bold(),
                )],
            ));
            if let Some(source) = resource.metadata.annotations.get("derived-from") {
                let how = resource
                    .metadata
                    .annotations
                    .get("derivation")
                    .map_or("derived", String::as_str);
                parts.push((
                    5,
                    vec![
                        Span::styled(format!("↳ {how} from "), Style::new().fg(DIM)),
                        Span::styled(format!("{source}  "), Style::new().fg(ICE)),
                    ],
                ));
            }
            if let Some(seed) = resource.spec.seed {
                let text = seed.to_string();
                let tail = &text[text.len().saturating_sub(4)..];
                parts.push((
                    6,
                    vec![Span::styled(
                        format!("seed …{tail}  "),
                        Style::new().fg(DIM),
                    )],
                ));
            }
            let (verdict, color) = verdict(resource.status.verdict());
            parts.push((
                4,
                vec![Span::styled(format!("{verdict}  "), Style::new().fg(color))],
            ));
        }
        parts.push((
            3,
            vec![Span::styled(
                format!("{}  ", focus.id),
                Style::new().fg(DIM),
            )],
        ));
    }
    let available = (area.width as usize).saturating_sub(left + 2);
    let mut chosen: Vec<u8> = Vec::new();
    let mut used = 0;
    let mut by_priority: Vec<&(u8, Vec<Span>)> = parts.iter().collect();
    by_priority.sort_by_key(|(priority, _)| *priority);
    for (priority, spans) in by_priority {
        let width = width_of(spans);
        if used + width <= available {
            used += width;
            chosen.push(*priority);
        }
    }
    // Display order: name, id, lineage, seed, phase, verdict, connection.
    let order = [2u8, 3, 5, 6, 1, 4, 0];
    let mut right = Vec::new();
    for priority in order {
        if chosen.contains(&priority) {
            if let Some((_, spans)) = parts.iter().find(|(p, _)| *p == priority) {
                right.extend(spans.iter().cloned());
            }
        }
    }
    frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), area);
}

fn phase(focus: &Focus) -> (String, Color) {
    if focus.terminal() {
        return ("■ completed".into(), DIM);
    }
    if let Some((p, _)) = &focus.playback {
        if p.state.stepping {
            return ("▷ stepping".into(), YELLOW);
        }
        if matches!(p.state.rate, PlaybackRate::Paused) {
            return ("❚❚ paused".into(), AMBER);
        }
        return ("● running".into(), GREEN);
    }
    match focus.resource.as_ref().map(|r| r.status.phase) {
        Some(Phase::Queued) => ("◌ queued".into(), AMBER),
        Some(Phase::Preparing) => ("◌ preparing".into(), YELLOW),
        Some(Phase::Running) => ("● running".into(), GREEN),
        Some(Phase::Finalizing) => ("◌ finalizing".into(), YELLOW),
        Some(Phase::Completed) => ("■ completed".into(), DIM),
        None => ("◌ connecting".into(), DIM),
    }
}

fn verdict(v: TestVerdict) -> (&'static str, Color) {
    match v {
        TestVerdict::Passed => ("passed", GREEN),
        TestVerdict::Failed => ("failed", RED),
        TestVerdict::Incomplete => ("incomplete", AMBER),
        TestVerdict::Pending => ("pending", DIM),
    }
}

fn footer(frame: &mut Frame, app: &App, area: Rect, now: Instant) {
    if let Some(name) = &app.timeline_name {
        frame.render_widget(
            Paragraph::new(format!(
                " Name: {name}▏  Enter applies · Esc cancels · Ctrl-U clears"
            ))
            .style(Style::new().fg(ICE)),
            area,
        );
        return;
    }
    if let Some(menu) = app.fault_menu {
        use crate::model::FaultMenu;
        let text = match menu {
            FaultMenu::Opening => "holding playback before opening the inject palette…",
            FaultMenu::Editing(_) => {
                "playback held while you choose; Esc cancels and restores the previous rate"
            }
            FaultMenu::Finishing => "waiting for the fault to be admitted…",
            FaultMenu::Failed => {
                "the control failed or was not acknowledged; playback was left as is. Esc to continue"
            }
        };
        frame.render_widget(
            Paragraph::new(format!(" {text}")).style(Style::new().fg(AMBER)),
            area,
        );
        return;
    }
    if let Some(notice) = app.notice(now) {
        let color = match notice.tone {
            Tone::Info => ICE,
            Tone::Good => GREEN,
            Tone::Bad => RED,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {}", notice.text),
                Style::new().fg(color),
            )),
            area,
        );
        return;
    }
    let mut keys: Vec<(String, String)> = Vec::new();
    let mut push = |key: &str, label: String| keys.push((key.to_owned(), label));
    match app.screen {
        Screen::Timeline => {
            for (k, l) in [
                ("↑↓", "select"),
                ("d", "remove/restore"),
                ("⏎", "run edited"),
                ("p", "pause before next"),
                ("N", "next fault"),
                ("w", "save definition"),
                ("n", "name"),
                ("u", "undo"),
                ("C", "clear"),
                ("R", "reset"),
                ("[]", "recording"),
                ("esc", "back"),
            ] {
                push(k, l.into());
            }
        }
        Screen::Simulation => match app.focus.as_ref() {
            Some(focus) if focus.starting().is_some() => {
                push("esc", "catalogue".into());
                push("3", "timeline".into());
                push("tab", "focus".into());
            }
            Some(focus) if focus.terminal() => {
                push("tab", "focus".into());
                push("r", "rerun".into());
                push("b", "branch".into());
                push("3", "edit timeline".into());
                push("esc", "catalogue".into());
            }
            Some(focus) => {
                let activity = focus.activity();
                push(
                    "space",
                    if focus.paused() { "resume" } else { "pause" }.into(),
                );
                push(".", "step".into());
                if activity.next(focus.now_ns(now)).is_some() {
                    push("n", "run to next fault".into());
                }
                if let Some(handle) = focus.healable(&activity) {
                    push("h", format!("heal {handle}"));
                }
                push(":", "inject".into());
                push("+-", "rate".into());
                push("c", "checkpoint".into());
                push("b", "branch".into());
                let panel = match app.panel {
                    Panel::Machines => "machines",
                    Panel::Faults => "faults",
                    Panel::Links => "links",
                    Panel::Logs => "logs",
                    Panel::Oracle => "oracle",
                };
                push("tab", format!("focus: {panel}"));
                push("v", "details".into());
            }
            None => {}
        },
        Screen::Catalogue => {
            for (k, l) in [
                ("↑↓", "select"),
                ("⏎", "open"),
                ("r", "rerun"),
                ("b", "branch"),
                ("s", "stop"),
                ("d", "delete"),
                ("m", "mark"),
                ("c", "compare"),
                ("q", "quit"),
            ] {
                push(k, l.into());
            }
        }
    }
    let help = vec![
        Span::styled(" ? ", Style::new().fg(ICE_BRIGHT).bg(SELECTED).bold()),
        Span::styled(" help ", Style::new().fg(DIM)),
    ];
    let budget = (area.width as usize).saturating_sub(width_of(&help));
    let mut spans = Vec::new();
    for (key, label) in keys {
        let pair = [
            Span::styled(
                format!(" {key} "),
                Style::new().fg(ICE_BRIGHT).bg(SELECTED).bold(),
            ),
            Span::styled(format!(" {label}  "), Style::new().fg(DIM)),
        ];
        if width_of(&spans) + width_of(&pair) > budget {
            break;
        }
        spans.extend(pair);
    }
    spans.extend(help);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn help(frame: &mut Frame, app: &App, area: Rect) {
    let lines: &[(&str, &str)] = match app.screen {
        Screen::Catalogue => &[
            ("↑ ↓  j k", "select a simulation"),
            ("⏎", "open it"),
            ("r", "rerun with the same retained inputs"),
            ("b", "branch from its newest checkpoint"),
            ("s / d", "stop / delete (press d twice)"),
            ("m then c", "mark a baseline, then compare another"),
            ("1 2 3", "catalogue · simulation · timeline"),
            ("q", "quit"),
        ],
        Screen::Timeline => &[
            ("↑ ↓", "select a recorded fault"),
            ("d", "remove or restore it in the edited timeline"),
            ("u / C / R", "undo · clear all · reset to recorded"),
            ("⏎", "run the edited timeline as a new simulation"),
            ("w", "save the edited definition to a file"),
            ("p / N", "pause before the next fault · select it"),
            ("[ ]", "switch between saved recordings"),
            ("esc", "back"),
        ],
        Screen::Simulation => &[
            ("space", "pause or resume"),
            (".", "step one scheduler round"),
            ("+ / -", "change the playback rate"),
            ("n", "run until just before the next fault"),
            (": or f", "inject a fault; heals are listed first"),
            ("h", "heal the selected (or oldest) active fault"),
            ("c / b", "checkpoint · branch from the newest checkpoint"),
            ("r", "rerun with the same retained inputs"),
            (
                "tab",
                "move focus: machines · faults · links · logs · oracle",
            ),
            ("↑ ↓  j k", "move within the focused panel"),
            ("a", "show every machine's logs"),
            ("e", "switch logs and raw server events"),
            ("[ ]", "zoom the time bar out / in"),
            ("PgUp PgDn", "scroll logs or the oracle · End follows again"),
            ("v", "technical details and request payloads"),
            ("1 2 3", "catalogue · simulation · timeline"),
            ("q", "quit"),
        ],
    };
    let key_width = lines
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(4)
        + 3;
    let text: Vec<Line> = lines
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(
                    format!(" {}", pad(k, key_width - 1)),
                    Style::new().fg(ICE).bold(),
                ),
                Span::styled(*v, Style::new().fg(TEXT)),
            ])
        })
        .collect();
    let width = (text.iter().map(Line::width).max().unwrap_or(40) as u16 + 3).min(area.width);
    let height = (text.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text)
            .block(block("keys", true).title_bottom(
                Line::styled(" any key closes ", Style::new().fg(DIM)).right_aligned(),
            ))
            .style(Style::new().bg(BAR)),
        rect,
    );
}

// ------------------------------------------------------------------ simulation

fn simulation(frame: &mut Frame, app: &App, focus: &Focus, area: Rect, now: Instant) {
    let activity = focus.activity();
    let sim_now = focus.now_ns(now);
    let present: Vec<String> = focus.machines();
    // Every machine gets a lane for its traffic, then fault handles and the rest.
    let mut lanes: Vec<Lane> = present.iter().cloned().map(Lane::Machine).collect();
    for lane in activity.lanes(focus.world.as_ref()) {
        if !lanes.contains(&lane) {
            lanes.push(lane);
        }
    }
    let wide = area.width >= 130 && area.height >= 34;
    // Tall terminals give the time bar room for more lanes; what still does
    // not fit is named on its last lane row.
    let timebar_height = if area.height >= 30 {
        (lanes.len() as u16 + 4).clamp(5, (area.height / 4).max(10))
    } else {
        (lanes.len() as u16 + 3).clamp(4, 5)
    };
    let [main, logs_area, time_area, status] = Layout::vertical([
        Constraint::Percentage(if area.height >= 40 { 54 } else { 50 }),
        Constraint::Min(3),
        Constraint::Length(timebar_height),
        Constraint::Length(1),
    ])
    .areas(area);
    let side_width = if wide {
        (area.width * 36 / 100).clamp(54, 74)
    } else {
        (area.width * 42 / 100).clamp(28, 48)
    };
    let [map_area, side] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(side_width)]).areas(main);
    match focus.starting() {
        Some(phase) => startup(frame, app, focus, phase, map_area),
        None => {
            map(frame, app, focus, &activity, map_area, sim_now);
            app.map_bottom.set(Some(map_area.y + map_area.height));
        }
    }
    if wide && app.panel == Panel::Links {
        // Focusing links expands them over the side panels.
        links(frame, app, focus, side, None);
    } else if wide {
        let machine_rows = (present.len() as u16 + 3).clamp(4, side.height / 2);
        // Faults take what their rows need; links fill the rest of the side.
        let fault_rows = (fault_entries(&activity) as u16 + 2).clamp(5, side.height / 3);
        let [faults_area, links_area, machines_area] = Layout::vertical([
            Constraint::Length(fault_rows),
            Constraint::Min(4),
            Constraint::Length(machine_rows),
        ])
        .areas(side);
        faults(frame, app, focus, &activity, faults_area, sim_now, None);
        links(frame, app, focus, links_area, None);
        machines(frame, app, focus, machines_area, None);
    } else if app.panel == Panel::Machines {
        machines(frame, app, focus, side, Some(Panel::Machines));
    } else if app.panel == Panel::Links {
        links(frame, app, focus, side, Some(Panel::Links));
    } else {
        faults(
            frame,
            app,
            focus,
            &activity,
            side,
            sim_now,
            Some(Panel::Faults),
        );
    }
    let failure = app
        .inspector(now)
        .filter(|inspector| app.details || !inspector.ok);
    // The oracle drives the test, so on wide terminals its output gets its own
    // panel beside the application logs.
    let oracle = focus
        .world
        .as_ref()
        .and_then(|w| w.machines.iter().find(|m| m.oracle))
        .map(|m| m.name.clone())
        .filter(|_| {
            failure.is_none()
                && !app.events_tab
                && focus.selected.is_none()
                && logs_area.width >= 120
                && logs_area.height >= 5
        });
    let (logs_area, oracle_area) = match &oracle {
        Some(_) => {
            let width = (logs_area.width * 34 / 100).clamp(40, 72);
            let [left, right] =
                Layout::horizontal([Constraint::Min(40), Constraint::Length(width)])
                    .areas(logs_area);
            (left, Some(right))
        }
        None => (logs_area, None),
    };
    app.logs_exclude.replace(oracle.clone());
    app.hitboxes.borrow_mut().push((logs_area, HitTarget::Logs));
    if let (Some(name), Some(area)) = (&oracle, oracle_area) {
        oracle_logs(frame, app, focus, name, area);
    }
    match failure {
        Some(inspector) => {
            let color = if inspector.ok { GREEN } else { RED };
            let mut lines = vec![
                Line::styled(format!("← {}", inspector.result), Style::new().fg(color)),
                Line::raw(""),
            ];
            if app.details {
                lines.extend(
                    inspector
                        .request
                        .lines()
                        .map(|line| Line::styled(line.to_owned(), Style::new().fg(TEXT))),
                );
            }
            frame.render_widget(Clear, logs_area);
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    block("request", true).title(
                        Line::styled(format!(" {} ", inspector.title), Style::new().fg(ICE))
                            .right_aligned(),
                    ),
                ),
                logs_area,
            );
        }
        None if app.events_tab => events(
            frame,
            focus,
            logs_area,
            app.details,
            app.events_scroll,
            app.panel == Panel::Logs,
        ),
        None => logs(frame, app, focus, logs_area),
    }
    crate::timebar::TimeBar {
        activity: &activity,
        lanes: &lanes,
        present: &present,
        checkpoints: &focus.checkpoints,
        traffic: &focus.traffic.history,
        selected: focus.fault_selected.as_ref(),
        selected_machine: focus.selected.as_deref(),
        now: sim_now,
        zoom: app.zoom,
        focused: app.panel == Panel::Faults,
    }
    .draw(frame, time_area, &app.hitboxes);
    status_bar(frame, focus, &activity, status, sim_now, app.details);
}

/// Blend smoothly between two colours over `period_ms`, for things in progress.
fn breathe(a: Color, b: Color, period_ms: u128) -> Color {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let t = ((ms % period_ms) as f64 / period_ms as f64 * std::f64::consts::TAU).sin() * 0.5 + 0.5;
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix =
                |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ => a,
    }
}

/// A light sweeping back and forth across `width` cells, for a machine that
/// is still booting or restoring.
fn scanner(width: usize) -> Vec<Span<'static>> {
    let width = width.max(4);
    let bounce = 2 * width - 2;
    let position = tick(45) % bounce;
    let head = if position < width {
        position
    } else {
        bounce - position
    };
    (0..width)
        .map(|i| {
            let color = match i.abs_diff(head) {
                0 => ICE_BRIGHT,
                1..=2 => ICE,
                3..=5 => ICE_MID,
                6..=9 => ICE_DIM,
                _ => FAINT,
            };
            Span::styled("━", Style::new().fg(color))
        })
        .collect()
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn spinner() -> char {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    SPINNER[(ms / 90) as usize % SPINNER.len()]
}

fn startup_paused(focus: &Focus) -> bool {
    focus.paused()
        && focus
            .playback
            .as_ref()
            .is_some_and(|(p, _)| !p.state.stepping)
}

/// In place of the map while a simulation is queued, preparing or booting:
/// what it is waiting for, progress so far and how long it has taken.
fn startup(frame: &mut Frame, app: &App, focus: &Focus, phase: Phase, area: Rect) {
    use seedbox_api::{PreparationStage, Stage};
    let title = focus.resource.as_ref().map_or("network".to_string(), |r| {
        format!("network · {}", simulation_name(r))
    });
    let b = block(&title, app.panel == Panel::Machines);
    let inner = b.inner(area);
    frame.render_widget(b, area);
    let Some(resource) = &focus.resource else {
        return;
    };
    let stage_elapsed = |stage: Stage| {
        resource
            .status
            .stage_timings
            .get(&stage)
            .map(|t| duration(t.elapsed_wall_ns))
    };
    let spin = spinner();
    let heading = |text: String| {
        Line::from(vec![
            Span::styled(format!("{spin}  "), Style::new().fg(ICE).bold()),
            Span::styled(text, Style::new().fg(WHITE).bold()),
        ])
    };
    let mut lines: Vec<Line> = Vec::new();
    match phase {
        Phase::Queued => {
            lines.push(heading("waiting for a free slot".into()));
            lines.push(Line::raw(""));
            let running: Vec<&SimulationSummary> = app
                .list
                .iter()
                .filter(|s| s.status.phase == Phase::Running && s.metadata.id != focus.id)
                .collect();
            if running.is_empty() {
                lines.push(Line::styled(
                    "the server will start it shortly",
                    Style::new().fg(DIM),
                ));
            } else {
                lines.push(Line::styled(
                    format!(
                        "{} simulation{} running on this server:",
                        running.len(),
                        if running.len() == 1 { " is" } else { "s are" }
                    ),
                    Style::new().fg(DIM),
                ));
                for s in running.iter().take(6) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("   {}  ", s.metadata.id), Style::new().fg(ICE)),
                        Span::styled(format!("{:<16}", s.metadata.name), Style::new().fg(TEXT)),
                        Span::styled(
                            format!("{:<10}", short_time(s.status.simulated_time_ns)),
                            Style::new().fg(DIM),
                        ),
                        Span::styled(
                            if s.status
                                .playback
                                .as_ref()
                                .is_some_and(|p| matches!(p.state.rate, PlaybackRate::Paused))
                            {
                                "❚❚ paused, still holding its slot"
                            } else {
                                ""
                            },
                            Style::new().fg(AMBER),
                        ),
                    ]));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "it starts when one finishes · to stop one: 1, select it, s",
                    Style::new().fg(FAINT_TEXT),
                ));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("queued for {}", age(resource.metadata.created_at_unix_ms)),
                Style::new().fg(AMBER),
            ));
        }
        Phase::Preparing => {
            lines.push(heading("preparing images and guests".into()));
            lines.push(Line::raw(""));
            let mut names: Vec<String> = focus.machines();
            for name in focus.preparation.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(4) + 2;
            for name in &names {
                let stage = focus.preparation.get(name);
                let step = |label: &str, done: bool, current: bool| {
                    let (mark, color) = if done {
                        ("✓", GREEN)
                    } else if current {
                        (&*spin.to_string(), ICE)
                    } else {
                        ("·", FAINT_TEXT)
                    };
                    Span::styled(format!("{mark} {label}   "), Style::new().fg(color))
                };
                let (image, guest, ready) = match stage {
                    None => (false, false, false),
                    Some(PreparationStage::Image) => (false, false, false),
                    Some(PreparationStage::Guest) => (true, false, false),
                    Some(PreparationStage::Ready) => (true, true, true),
                };
                lines.push(Line::from(vec![
                    Span::styled(pad(name, width), Style::new().fg(machine_color(name))),
                    step("image", image, !image),
                    step("guest", guest, image && !guest),
                    step("ready", ready, false),
                ]));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(
                    "preparing for {}",
                    stage_elapsed(Stage::Preparing)
                        .unwrap_or_else(|| age(resource.metadata.created_at_unix_ms))
                ),
                Style::new().fg(YELLOW),
            ));
        }
        _ => {
            if startup_paused(focus) {
                lines.push(Line::styled(
                    "❚❚  startup paused",
                    Style::new().fg(AMBER).bold(),
                ));
            } else {
                lines.push(heading("booting machines".into()));
            }
            lines.push(Line::raw(""));
            if let Some(world) = &focus.world {
                let width = world
                    .machines
                    .iter()
                    .map(|m| m.name.chars().count())
                    .max()
                    .unwrap_or(4)
                    + 2;
                for m in &world.machines {
                    let (state, color) = condition(focus, m);
                    lines.push(Line::from(vec![
                        Span::styled(pad(&m.name, width), Style::new().fg(machine_color(&m.name))),
                        Span::styled(state, Style::new().fg(color)),
                    ]));
                }
            }
            if startup_paused(focus) {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Press Space to resume startup",
                    Style::new().fg(TEXT),
                ));
            } else if let Some(elapsed) = stage_elapsed(Stage::Booting) {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("booting for {elapsed}"),
                    Style::new().fg(YELLOW),
                ));
            }
        }
    }
    let height = (lines.len() as u16).min(inner.height);
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let rect = Rect::new(
        inner.x + inner.width.saturating_sub(width) / 2,
        inner.y + inner.height.saturating_sub(height) / 2,
        width.min(inner.width),
        height,
    );
    frame.render_widget(Paragraph::new(lines), rect);
}

/// Slot centres on an ellipse, in cell coordinates relative to `area`. Slots
/// are assigned in first-seen order so machines keep their place.
pub fn layout(slots: usize, area: Rect, box_width: u16, box_height: u16) -> Vec<(f64, f64)> {
    let (cx, cy) = (area.width as f64 / 2.0, area.height as f64 / 2.0);
    if slots == 1 {
        return vec![(cx, cy)];
    }
    let rx = (cx - box_width as f64 / 2.0 - 1.0).max(2.0);
    let ry = (cy - box_height as f64 / 2.0).max(1.0);
    (0..slots)
        .map(|i| {
            let angle =
                -std::f64::consts::FRAC_PI_2 + i as f64 * std::f64::consts::TAU / slots as f64;
            (cx + rx * angle.cos(), cy + ry * angle.sin())
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pattern {
    Solid,
    /// Dashes that march along the link, for cut links.
    Marching,
    Dotted,
}

/// Wall-clock phase for animation, in steps of `period_ms`.
pub(crate) fn tick(period_ms: u128) -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| (d.as_millis() / period_ms) as usize)
}

/// A gentle quadratic curve between two machines. Curves bow away from the
/// centre of the map so the middle stays readable.
#[derive(Clone, Copy)]
struct Curve {
    from: (f64, f64),
    control: (f64, f64),
    to: (f64, f64),
}

impl Curve {
    fn new(from: (f64, f64), to: (f64, f64), centre: (f64, f64), bias: f64) -> Self {
        // Work in visual space, where a cell is about twice as tall as wide,
        // so curves bow by the same amount whatever their direction.
        const ASPECT: f64 = 2.0;
        let visual = |p: (f64, f64)| (p.0, p.1 * ASPECT);
        let (from_v, to_v, centre_v) = (visual(from), visual(to), visual(centre));
        let control = Self::control(from_v, to_v, centre_v, bias);
        Self {
            from,
            control: (control.0, control.1 / ASPECT),
            to,
        }
    }

    fn control(from: (f64, f64), to: (f64, f64), centre: (f64, f64), bias: f64) -> (f64, f64) {
        let mid = ((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let length = (dx * dx + dy * dy).sqrt().max(1e-9);
        let normal = (-dy / length, dx / length);
        let outward = (mid.0 - centre.0) * normal.0 + (mid.1 - centre.1) * normal.1;
        let sign = if outward.abs() < length * 0.05 {
            bias
        } else {
            outward.signum()
        };
        let bow = length * 0.1 * sign;
        (mid.0 + normal.0 * bow, mid.1 + normal.1 * bow)
    }

    fn at(&self, t: f64) -> (f64, f64) {
        let u = 1.0 - t;
        (
            u * u * self.from.0 + 2.0 * u * t * self.control.0 + t * t * self.to.0,
            u * u * self.from.1 + 2.0 * u * t * self.control.1 + t * t * self.to.1,
        )
    }

    fn segments(&self, n: usize) -> Vec<((f64, f64), (f64, f64))> {
        (0..n)
            .map(|i| {
                (
                    self.at(i as f64 / n as f64),
                    self.at((i + 1) as f64 / n as f64),
                )
            })
            .collect()
    }

    /// Where this curve crosses another, if it does.
    fn crossings(&self, other: &Curve) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        for a in self.segments(24) {
            for b in other.segments(24) {
                if let Some(p) = intersection(a, b) {
                    out.push(p);
                }
            }
        }
        out
    }
}

/// A link drawn on the braille canvas: a curve whose weight follows traffic.
/// Lines that cross an earlier link leave a small gap, so crossings read as
/// bridges rather than knots.
struct Link {
    curve: Curve,
    color: Color,
    pattern: Pattern,
    weight: u8,
    gaps: Vec<(f64, f64)>,
}

impl Shape for Link {
    fn draw(&self, painter: &mut Painter) {
        let (Some(a), Some(c), Some(b)) = (
            painter.get_point(self.curve.from.0, self.curve.from.1),
            painter.get_point(self.curve.control.0, self.curve.control.1),
            painter.get_point(self.curve.to.0, self.curve.to.1),
        ) else {
            return;
        };
        let distance =
            |p: (usize, usize), q: (usize, usize)| p.0.abs_diff(q.0).max(p.1.abs_diff(q.1));
        let steps = (distance(a, c) + distance(c, b)).max(1);
        let gaps: Vec<(usize, usize)> = self
            .gaps
            .iter()
            .filter_map(|p| painter.get_point(p.0, p.1))
            .collect();
        let march = tick(110);
        let mut points: Vec<(usize, usize)> = Vec::with_capacity(steps + 1);
        for i in 0..=steps {
            let visible = match self.pattern {
                Pattern::Solid => true,
                Pattern::Marching => (i + steps * 6 - march % 6) % 6 < 4,
                Pattern::Dotted => i % 4 == 0,
            };
            if !visible {
                continue;
            }
            let (x, y) = self.curve.at(i as f64 / steps as f64);
            let Some(point) = painter.get_point(x, y) else {
                continue;
            };
            if points.last() == Some(&point)
                || gaps
                    .iter()
                    .any(|(gx, gy)| gx.abs_diff(point.0) <= 3 && gy.abs_diff(point.1) <= 3)
            {
                continue;
            }
            points.push(point);
        }
        // Heavier links add a dot beside the line, perpendicular to its run,
        // so weight reads as thickness rather than a solid block.
        let steep = a.0.abs_diff(b.0) < a.1.abs_diff(b.1);
        for (i, (x, y)) in points.into_iter().enumerate() {
            painter.paint(x, y, self.color);
            if self.weight >= 2 {
                let beside = if steep { (x + 1, y) } else { (x, y + 1) };
                painter.paint(beside.0, beside.1, self.color);
            }
            if self.weight >= 3 && i % 2 == 0 {
                let other = if steep {
                    (x.saturating_sub(1), y)
                } else {
                    (x, y.saturating_sub(1))
                };
                painter.paint(other.0, other.1, self.color);
            }
        }
    }
}

fn intersection(a: ((f64, f64), (f64, f64)), b: ((f64, f64), (f64, f64))) -> Option<(f64, f64)> {
    let ((x1, y1), (x2, y2)) = a;
    let ((x3, y3), (x4, y4)) = b;
    let denominator = (x1 - x2) * (y3 - y4) - (y1 - y2) * (x3 - x4);
    if denominator.abs() < 1e-9 {
        return None;
    }
    let t = ((x1 - x3) * (y3 - y4) - (y1 - y3) * (x3 - x4)) / denominator;
    let u = -((x1 - x2) * (y1 - y3) - (y1 - y2) * (x1 - x3)) / denominator;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u))
        .then_some((x1 + t * (x2 - x1), y1 + t * (y2 - y1)))
}

fn down(m: &MachineState) -> bool {
    matches!(
        m.condition,
        MachineCondition::Crashed | MachineCondition::Exited
    )
}

fn traffic_color(per_second: f64, involved: bool) -> Color {
    let level = match per_second {
        r if r < 0.5 => 0,
        r if r < 20.0 => 1,
        r if r < 200.0 => 2,
        _ => 3,
    } + usize::from(involved);
    [FAINT, ICE_DIM, ICE_MID, ICE, ICE_BRIGHT][level.min(4)]
}

/// Line weight on a log scale relative to the busiest link.
fn traffic_weight(per_second: f64, busiest: f64) -> u8 {
    if per_second < 0.5 || busiest < 0.5 {
        return 1;
    }
    let share = (per_second + 1.0).ln() / (busiest + 1.0).ln();
    1 + (share * 2.0).round() as u8
}

fn map(frame: &mut Frame, app: &App, focus: &Focus, activity: &Activity, area: Rect, now: u64) {
    let in_flight = focus
        .frames
        .iter()
        .filter(|f| f.fate == FrameFate::Scheduled)
        .count();
    let title = focus.resource.as_ref().map_or("network".to_string(), |r| {
        format!("network · {}", simulation_name(r))
    });
    let outer = block(&title, app.panel == Panel::Machines)
        .title(
            Line::styled(format!(" {in_flight} in flight "), Style::new().fg(DIM)).right_aligned(),
        )
        .title_bottom(source("GET world · frames", app.details));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let Some(world) = &focus.world else {
        frame.render_widget(Paragraph::new("waiting for the world…").fg(DIM), inner);
        return;
    };
    if inner.width < 8 || inner.height < 3 {
        return;
    }
    let effects: Vec<Vec<&Item>> = world
        .machines
        .iter()
        .map(|machine| {
            activity
                .rows()
                .filter(|item| item.open() && item.lane == Lane::Machine(machine.name.clone()))
                .collect()
        })
        .collect();
    let effect_rows = effects.iter().map(Vec::len).max().unwrap_or(0);
    let effect_width = effects
        .iter()
        .flatten()
        .map(|item| effect_label(item, now).chars().count() as u16 + 2)
        .max()
        .unwrap_or(0);
    let compact = inner.width < 64 || inner.height < 16;
    let (box_width, box_height) = if compact {
        (
            world
                .machines
                .iter()
                .map(|m| m.name.chars().count() as u16 + 6)
                .max()
                .unwrap_or(8),
            1,
        )
    } else {
        (
            world
                .machines
                .iter()
                .map(|m| m.name.chars().count() as u16 + 8)
                .max()
                .unwrap_or(22)
                .max(22)
                .max(effect_width),
            5u16.saturating_add(effect_rows.max(1).min(u16::MAX as usize) as u16),
        )
    };
    let field = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let slots: Vec<&String> = focus.seen.iter().collect();
    let centres = layout(slots.len().max(1), field, box_width, box_height);
    let centre = |name: &str| {
        slots
            .iter()
            .position(|n| n.as_str() == name)
            .and_then(|i| centres.get(i).copied())
    };
    let (w, h) = (inner.width as f64, inner.height as f64);
    let to_canvas = |(x, y): (f64, f64)| (x, h - y);
    let selected = focus.selected.as_deref();
    let targets: Vec<String> = app
        .fault_form
        .as_ref()
        .map(|form| form.targets())
        .unwrap_or_default();

    // Undirected pairs; the worse direction decides the look.
    let machine = |name: &str| world.machines.iter().find(|m| m.name == name);
    let mut pairs: Vec<(String, String)> = Vec::new();
    for link in &world.links {
        if machine(&link.from).is_none() || machine(&link.to).is_none() {
            continue;
        }
        let key = if link.from < link.to {
            (link.from.clone(), link.to.clone())
        } else {
            (link.to.clone(), link.from.clone())
        };
        if !pairs.contains(&key) {
            pairs.push(key);
        }
    }
    let busiest = pairs
        .iter()
        .map(|(a, b)| focus.traffic.link_rate(a, b) + focus.traffic.link_rate(b, a))
        .fold(0.0f64, f64::max);
    let map_centre = (w / 2.0, h / 2.0);
    let mut drawn: Vec<Link> = Vec::new();
    let mut curves: Vec<((String, String), Curve)> = Vec::new();
    let mut labels: Vec<((f64, f64), String, Color)> = Vec::new();
    let pulse = tick(500).is_multiple_of(2);
    for (index, (a, b)) in pairs.iter().enumerate() {
        let (Some(ca), Some(cb)) = (centre(a), centre(b)) else {
            continue;
        };
        let both: Vec<_> = world
            .links
            .iter()
            .filter(|l| (&l.from == a && &l.to == b) || (&l.from == b && &l.to == a))
            .collect();
        let cut = both.iter().find_map(|l| l.partitioned_by.clone());
        let shaped = both.iter().find(|l| l.shaped_by.is_some());
        let is_down = machine(a).is_some_and(down) || machine(b).is_some_and(down);
        let involved = selected.is_some_and(|s| s == a || s == b);
        let traffic = focus.traffic.link_rate(a, b) + focus.traffic.link_rate(b, a);
        let previewed = app
            .fault_form
            .as_ref()
            .is_some_and(|form| form.targets_link(a, b) || form.targets_link(b, a));
        let (color, pattern, weight) = if previewed {
            (MAGENTA, Pattern::Marching, 2)
        } else if cut.is_some() {
            (if pulse { RED } else { RED_SOFT }, Pattern::Marching, 1)
        } else if is_down {
            (FAINT, Pattern::Dotted, 1)
        } else if shaped.is_some() {
            (YELLOW, Pattern::Solid, traffic_weight(traffic, busiest))
        } else {
            (
                traffic_color(traffic, involved),
                Pattern::Solid,
                traffic_weight(traffic, busiest),
            )
        };
        let bias = if index % 2 == 0 { 1.0 } else { -1.0 };
        let curve = Curve::new(to_canvas(ca), to_canvas(cb), map_centre, bias);
        let gaps = drawn
            .iter()
            .flat_map(|other| curve.crossings(&other.curve))
            .collect();
        // Labels sit off-centre so they avoid the middle, where lines cross.
        let (lx, ly) = curve.at(0.36);
        let at = (lx, ly + 0.6);
        if let Some(handle) = &cut {
            // One label per handle; a partition cuts many links.
            let text = format!("✂ {handle}");
            if !labels.iter().any(|(_, existing, _)| *existing == text) {
                labels.push((at, text, RED));
            }
        } else if let Some(link) = shaped {
            labels.push((at, format!("≈ {}", latency(link.policy.latency_ns)), YELLOW));
        } else if involved && !is_down {
            labels.push((at, format!("{}/s", rate(traffic)), ICE));
        }
        curves.push(((a.clone(), b.clone()), curve));
        drawn.push(Link {
            curve,
            color,
            pattern,
            weight,
            gaps,
        });
    }
    // Frames travel the same curve as their link, in either direction.
    let along = |from: &str, to: &str, t: f64| {
        curves.iter().find_map(|((a, b), curve)| {
            if a == from && b == to {
                Some(curve.at(t))
            } else if a == to && b == from {
                Some(curve.at(1.0 - t))
            } else {
                None
            }
        })
    };
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, w])
        .y_bounds([0.0, h])
        .paint(|ctx| {
            for link in &drawn {
                ctx.draw(link);
            }
            ctx.layer();
            for ((x, y), text, color) in &labels {
                let half = text.chars().count() as f64 / 2.0;
                ctx.print(
                    x - half,
                    *y,
                    Span::styled(text.clone(), Style::new().fg(*color).bold()),
                );
            }
            for frame in &focus.frames {
                let span = frame
                    .due_ns
                    .unwrap_or(frame.sent_ns + 1_000_000)
                    .saturating_sub(frame.sent_ns)
                    .max(1);
                let mut progress = now.saturating_sub(frame.sent_ns) as f64 / span as f64;
                let (glyph, color) = match frame.fate {
                    FrameFate::Scheduled => ("●", frame_color(frame.bytes)),
                    FrameFate::PartitionDropped => {
                        progress = progress.min(0.5);
                        ("✕", RED)
                    }
                    FrameFate::LossDropped | FrameFate::QueueDropped => {
                        progress = progress.min(0.6);
                        ("×", YELLOW)
                    }
                    FrameFate::ReceiverDropped => {
                        progress = progress.min(0.95);
                        ("✕", ORANGE)
                    }
                };
                let progress = progress.clamp(0.0, 1.0);
                let (lo, hi) = (0.2, 0.8);
                let t = lo + (hi - lo) * progress;
                if frame.fate == FrameFate::Scheduled && progress > 0.06 {
                    if let Some((x, y)) =
                        along(&frame.from, &frame.to, lo + (hi - lo) * (progress - 0.06))
                    {
                        ctx.print(x, y, Span::styled("•", Style::new().fg(dim(color))));
                    }
                }
                if let Some((x, y)) = along(&frame.from, &frame.to, t) {
                    ctx.print(x, y, Span::styled(glyph, Style::new().fg(color).bold()));
                }
            }
        });
    frame.render_widget(canvas, inner);

    for (index, m) in world.machines.iter().enumerate() {
        let Some(c) = centre(&m.name) else {
            continue;
        };
        let target = targets.contains(&m.name);
        let height = if compact {
            1
        } else {
            5u16.saturating_add(effects[index].len().max(1).min(u16::MAX as usize) as u16)
        };
        let rect = node_rect(c, field, box_width, height);
        app.hitboxes
            .borrow_mut()
            .push((rect, HitTarget::Machine(index)));
        if compact {
            badge(frame, focus, m, rect, target);
        } else {
            node(frame, focus, m, rect, &effects[index], target, now);
        }
    }
    if inner.height > 2 && inner.width > 4 {
        let key = |mark: &'static str, color: Color, label: &'static str| {
            vec![
                Span::styled(mark, Style::new().fg(color)),
                Span::styled(label, Style::new().fg(FAINT_TEXT)),
            ]
        };
        let legend: Vec<Vec<Span>> = vec![
            key("━ ", ICE, "traffic  "),
            key("┅ ", RED, "cut  "),
            key("┈ ", DIM, "down  "),
            key("━ ", YELLOW, "shaped  "),
            vec![
                Span::styled("●", Style::new().fg(TEAL)),
                Span::styled("●", Style::new().fg(ICE)),
                Span::styled("● ", Style::new().fg(AMBER)),
                Span::styled("frames  ", Style::new().fg(FAINT_TEXT)),
            ],
            key("✕ ", RED, "dropped"),
        ];
        let mut legend = legend;
        if app.fault_form.is_some() {
            legend.insert(0, key("┅ ", MAGENTA, "previewed  "));
        }
        // Whole legend entries only: drop trailing entries that do not fit.
        let budget = inner.width.saturating_sub(2) as usize;
        let mut fitted: Vec<Span> = Vec::new();
        for entry in legend {
            if width_of(&fitted) + width_of(&entry) > budget {
                break;
            }
            fitted.extend(entry);
        }
        frame.render_widget(
            Paragraph::new(Line::from(fitted)),
            Rect::new(inner.x + 1, inner.y + inner.height - 1, inner.width - 2, 1),
        );
    }
}

fn node_rect(centre: (f64, f64), field: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(field.width);
    let height = height.min(field.height.max(1));
    let x = (field.x as f64 + centre.0 - width as f64 / 2.0)
        .round()
        .max(field.x as f64) as u16;
    let y = (field.y as f64 + centre.1 - height as f64 / 2.0)
        .round()
        .max(field.y as f64) as u16;
    Rect::new(
        x.min(field.x + field.width - width),
        y.min(field.y + field.height.saturating_sub(height)),
        width,
        height,
    )
}

/// Frames are coloured by size: control chatter, ordinary requests, bulk data.
fn frame_color(bytes: u32) -> Color {
    match bytes {
        0..=128 => TEAL,
        129..=1024 => ICE,
        _ => AMBER,
    }
}

pub(crate) fn dim(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(r / 2 + 4, g / 2 + 6, b / 2 + 9),
        other => other,
    }
}

fn isolated(focus: &Focus, name: &str) -> bool {
    focus.world.as_ref().is_some_and(|w| {
        let mine: Vec<_> = w
            .links
            .iter()
            .filter(|l| l.from == name || l.to == name)
            .collect();
        !mine.is_empty() && mine.iter().all(|l| l.partitioned_by.is_some())
    })
}

fn node_border(focus: &Focus, m: &MachineState, target: bool) -> (Color, bool) {
    let selected = focus.selected.as_deref() == Some(m.name.as_str());
    let color = if target {
        MAGENTA
    } else if selected {
        ICE
    } else if down(m) || isolated(focus, &m.name) {
        RED
    } else if m.condition == MachineCondition::Paused {
        AMBER
    } else if m.oracle {
        Color::Rgb(91, 63, 122)
    } else {
        BORDER_HI
    };
    (color, selected)
}

fn badge(frame: &mut Frame, focus: &Focus, m: &MachineState, rect: Rect, target: bool) {
    let (state, mut color) = condition(focus, m);
    let (border, selected) = node_border(focus, m, target);
    if m.condition == MachineCondition::Starting && !focus.terminal() && !startup_paused(focus) {
        color = breathe(ICE_DIM, ICE_BRIGHT, 1600);
    }
    let glyph = state.chars().next().unwrap_or('●');
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", m.name),
            Style::new()
                .fg(if selected {
                    WHITE
                } else {
                    machine_color(&m.name)
                })
                .bold(),
        ),
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
    ]);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(line).style(
            Style::new()
                .bg(if selected { SELECTED } else { NODE })
                .fg(border),
        ),
        rect,
    );
}

fn effect_label(item: &Item, now: u64) -> String {
    let handle = item
        .handle
        .as_ref()
        .map(|handle| format!(" {handle}"))
        .unwrap_or_default();
    format!(
        "{} {}{handle} · {}",
        glyph(&item.action, item.state),
        item.action,
        duration(now.saturating_sub(item.start_ns))
    )
}

fn node(
    frame: &mut Frame,
    focus: &Focus,
    m: &MachineState,
    rect: Rect,
    effects: &[&Item],
    target: bool,
    now: u64,
) {
    let (mut border, selected) = node_border(focus, m, target);
    let starting =
        m.condition == MachineCondition::Starting && !focus.terminal() && !startup_paused(focus);
    let active: Vec<_> = effects.iter().copied().filter(|item| item.open()).collect();
    if !target && !selected && !active.is_empty() && tick(500).is_multiple_of(2) {
        border = span_color(&active[0].action, false);
    } else if !target && !selected && starting {
        border = breathe(BORDER_HI, ICE, 1600);
    }
    let (state, color) = condition(focus, m);
    let mut title = vec![Span::styled(
        format!(" {} ", m.name),
        Style::new()
            .fg(if selected {
                WHITE
            } else {
                machine_color(&m.name)
            })
            .bold(),
    )];
    if m.oracle {
        title.push(Span::styled("◆ ", Style::new().fg(MAGENTA)));
    }
    if isolated(focus, &m.name) {
        title.push(Span::styled("✂ ", Style::new().fg(RED)));
    }
    if target {
        title.push(Span::styled("◎ target ", Style::new().fg(MAGENTA).bold()));
    }
    let b = Block::bordered()
        .border_type(if selected || target {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(Style::new().fg(border))
        .title(Line::from(title))
        .style(Style::new().bg(if selected { SELECTED } else { NODE }));
    let mut first = vec![Span::styled(state, Style::new().fg(color).bold())];
    if m.incarnation > 0 {
        first.push(Span::styled(
            format!("  i{}", m.incarnation),
            Style::new().fg(YELLOW),
        ));
    }
    let inner_width = rect.width.saturating_sub(2) as usize;
    let tx = focus.traffic.sent_rate(&m.name);
    let rx = focus.traffic.received_rate(&m.name);
    let mut lines = vec![
        Line::from(first),
        Line::from(vec![
            Span::styled(
                format!("↑{}/s ", rate(tx)),
                Style::new().fg(if tx > 0.5 { ICE } else { DIM }),
            ),
            Span::styled(format!("↓{}/s", rate(rx)), Style::new().fg(DIM)),
        ]),
        if starting {
            Line::from(scanner(inner_width))
        } else {
            Line::styled(
                sparkline(focus, &m.name, inner_width),
                Style::new().fg(if down(m) {
                    FAINT
                } else {
                    dim(machine_color(&m.name))
                }),
            )
        },
    ];
    if starting && active.is_empty() {
        let detail = match focus
            .resource
            .as_ref()
            .and_then(|r| r.metadata.annotations.get("branch-at-ns"))
            .and_then(|ns| ns.parse::<u64>().ok())
        {
            Some(at) if focus.restoring() => format!("from checkpoint @ {}", short_time(at)),
            _ if focus.restoring() => "from a checkpoint".into(),
            _ => "waiting for its ready signal".into(),
        };
        lines.push(Line::styled(detail, Style::new().fg(FAINT_TEXT)));
    } else {
        for item in active {
            lines.push(Line::styled(
                effect_label(item, now),
                Style::new().fg(span_color(&item.action, true)),
            ));
        }
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(b), rect);
}

fn condition(focus: &Focus, m: &MachineState) -> (String, Color) {
    let mut text = match m.condition {
        MachineCondition::Starting if focus.terminal() => ("◌ never ready".to_string(), FAINT_TEXT),
        MachineCondition::Starting if startup_paused(focus) => ("❚❚ paused".to_string(), AMBER),
        MachineCondition::Starting => (
            format!(
                "{} {}",
                spinner(),
                if focus.restoring() {
                    "restoring"
                } else {
                    "booting"
                }
            ),
            ICE,
        ),
        MachineCondition::Ready => ("● ready".to_string(), GREEN),
        MachineCondition::Paused => ("❚❚ paused".to_string(), AMBER),
        MachineCondition::Crashed => ("✕ crashed".to_string(), RED),
        MachineCondition::Exited => (
            format!("✕ exited {}", m.exit_code.unwrap_or(0)),
            if m.exit_code == Some(0) { DIM } else { RED },
        ),
    };
    if let Some(hz) = m.throttle_speed_hz.or(m.cpu_speed_hz) {
        text.0.push_str(&format!(" {}", frequency(hz)));
    }
    text
}

/// A CPU speed in its largest unit, with up to two decimals: `2GHz`,
/// `1.5GHz`, `333.33MHz`, `800kHz`.
pub fn frequency(hz: u64) -> String {
    for (unit, scale) in [
        ("GHz", 1_000_000_000u64),
        ("MHz", 1_000_000),
        ("kHz", 1_000),
    ] {
        if hz >= scale {
            let hundredths = (hz * 100).div_ceil(scale);
            let text = format!("{}.{:02}", hundredths / 100, hundredths % 100);
            return format!("{}{unit}", text.trim_end_matches('0').trim_end_matches('.'));
        }
    }
    format!("{hz}Hz")
}

fn tabbed_title(active: Panel) -> Line<'static> {
    let tab = |label: &'static str, on: bool| {
        Span::styled(
            format!(" {label} "),
            if on {
                Style::new().fg(ICE_BRIGHT).bold()
            } else {
                Style::new().fg(FAINT_TEXT)
            },
        )
    };
    Line::from(vec![
        tab("faults", active == Panel::Faults),
        Span::styled("│", Style::new().fg(BORDER)),
        tab("machines", active == Panel::Machines),
        Span::styled("│", Style::new().fg(BORDER)),
        tab("links", active == Panel::Links),
    ])
}

/// A short name for why frames were dropped.
fn drop_cause(cause: &str) -> &'static str {
    match cause {
        "partition" => "cut",
        "loss" => "loss",
        "queue" => "queue",
        _ => "rx",
    }
}

fn machines(frame: &mut Frame, app: &App, focus: &Focus, area: Rect, tabbed: Option<Panel>) {
    let focused = app.panel == Panel::Machines;
    let mut b = match tabbed {
        Some(panel) => Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { BORDER_HI } else { BORDER }))
            .title(tabbed_title(panel)),
        None => block("machines", focused),
    };
    b = b.title_bottom(source("GET world", app.details));
    let inner = b.inner(area);
    frame.render_widget(b, area);
    let Some(world) = &focus.world else {
        return;
    };
    let width = inner.width as usize;
    let name_width = world
        .machines
        .iter()
        .map(|m| m.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 14)
        + 1;
    // Every state carries the machine's CPU speed, so size the column to the
    // longest one rather than cutting a speed short.
    let states: Vec<(String, Color)> = world.machines.iter().map(|m| condition(focus, m)).collect();
    let state_width = states
        .iter()
        .map(|(state, _)| state.chars().count() + 1)
        .max()
        .unwrap_or(14)
        .clamp(14, 24);
    let base = name_width + 2 + state_width + 7 + 7;
    let show_drop = width >= base + 13;
    let show_spark = width >= base + 13 + 10;
    let mut header = vec![Span::styled(
        format!(
            "  {}{}{}{}",
            pad("NAME", name_width),
            pad("STATE", state_width),
            pad_left("TX/S", 7),
            pad_left("RX/S", 7)
        ),
        Style::new().fg(FAINT_TEXT),
    )];
    if show_drop {
        header.push(Span::styled(
            format!("{} {}", pad_left("DROP/S", 7), pad("CAUSE", 5)),
            Style::new().fg(FAINT_TEXT),
        ));
    }
    if show_spark {
        header.push(Span::styled("  ACTIVITY", Style::new().fg(FAINT_TEXT)));
    }
    let mut lines = vec![Line::from(header)];
    let spark_width = width.saturating_sub(base + 13 + 2);
    for (index, (m, (state, color))) in world.machines.iter().zip(states).enumerate() {
        let selected = focus.selected.as_deref() == Some(m.name.as_str());
        let dropped = focus.traffic.dropped_rate(&m.name);
        let mut spans = vec![
            Span::styled(if selected { "▸ " } else { "  " }, Style::new().fg(ICE)),
            Span::styled(
                pad(&m.name, name_width),
                Style::new()
                    .fg(if selected {
                        WHITE
                    } else {
                        machine_color(&m.name)
                    })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(pad(&state, state_width), Style::new().fg(color)),
            Span::styled(
                pad_left(&rate(focus.traffic.sent_rate(&m.name)), 7),
                Style::new().fg(TEXT),
            ),
            Span::styled(
                pad_left(&rate(focus.traffic.received_rate(&m.name)), 7),
                Style::new().fg(TEXT),
            ),
        ];
        if show_drop {
            let total = dropped.total();
            spans.push(Span::styled(
                pad_left(&rate(total), 7),
                Style::new().fg(if total >= 0.5 { RED } else { DIM }),
            ));
            let cause = dropped.dominant().map_or("", drop_cause);
            spans.push(Span::styled(
                format!(" {}", pad(cause, 5)),
                Style::new().fg(RED_SOFT),
            ));
        }
        if show_spark {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                sparkline(focus, &m.name, spark_width.min(16)),
                Style::new().fg(dim(machine_color(&m.name))),
            ));
        }
        let row = Rect::new(inner.x, inner.y + 1 + index as u16, inner.width, 1);
        if row.y < inner.y + inner.height {
            app.hitboxes
                .borrow_mut()
                .push((row, HitTarget::Machine(index)));
        }
        let mut line = Line::from(spans);
        if selected {
            line = line.style(Style::new().bg(SELECTED));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Rows the faults panel needs: section titles, their items and the hint.
fn fault_entries(activity: &Activity) -> usize {
    let section = |items: &[Item]| usize::from(!items.is_empty()) + items.len();
    let active = 1 + activity.active.len().max(1);
    let rest = section(&activity.upcoming) + section(&activity.history);
    let hint = if rest == 0 && activity.active.is_empty() {
        2
    } else {
        0
    };
    active + rest + hint
}

/// Bytes per simulated second, compact.
fn bytes_rate(per_second: f64) -> String {
    match per_second {
        r if r < 0.5 => "0".into(),
        r if r < 1024.0 => format!("{r:.0}B"),
        r if r < 1024.0 * 1024.0 => format!("{:.0}K", r / 1024.0),
        r => format!("{:.1}M", r / (1024.0 * 1024.0)),
    }
}

/// Every directed link with its policy, traffic, drops and any fault on it.
/// Links of the selected machine come first, then cut or shaped links, each
/// group by name so rows hold still while scrolling.
fn links(frame: &mut Frame, app: &App, focus: &Focus, area: Rect, tabbed: Option<Panel>) {
    let focused = app.panel == Panel::Links;
    let mut b = match tabbed {
        Some(panel) => Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { BORDER_HI } else { BORDER }))
            .title(tabbed_title(panel)),
        None => block("links", focused),
    }
    .title_bottom(source("GET world", app.details));
    let inner = b.inner(area);
    app.hitboxes.borrow_mut().push((area, HitTarget::Links));
    let Some(world) = &focus.world else {
        frame.render_widget(b, area);
        return;
    };
    let selected = focus.selected.as_deref();
    let mut links: Vec<_> = world.links.iter().collect();
    links.sort_by(|a, b| {
        let rank = |l: &seedbox_api::LinkState| {
            (
                !selected.is_some_and(|s| l.from == s || l.to == s),
                l.partitioned_by.is_none() && l.shaped_by.is_none(),
            )
        };
        rank(a)
            .cmp(&rank(b))
            .then((&a.from, &a.to).cmp(&(&b.from, &b.to)))
    });
    let rows = inner.height.saturating_sub(1) as usize;
    app.links_rows.set(rows);
    let start = app.links_scroll.min(links.len().saturating_sub(rows));
    let end = (start + rows).min(links.len());
    let summary = if end - start < links.len() {
        let hint = if focused { "↑↓" } else { "tab ↑↓" };
        format!(" {}–{} of {} links · {hint} ", start + 1, end, links.len())
    } else {
        format!(" {} links ", links.len())
    };
    let title_width = if tabbed.is_some() { 30 } else { 9 };
    let summary = Line::styled(summary, Style::new().fg(DIM)).right_aligned();
    b = if title_width + summary.width() + 2 <= area.width as usize {
        b.title(summary)
    } else {
        b.title_bottom(summary)
    };
    frame.render_widget(b, area);
    let width = inner.width as usize;
    let name_width = links
        .iter()
        .map(|l| l.from.chars().count() + l.to.chars().count() + 1)
        .max()
        .unwrap_or(6)
        .clamp(6, 18)
        + 1;
    let show_total = width >= name_width + 1 + 7 * 5 + 8;
    let fault_width = width.saturating_sub(name_width + 1 + 7 * 4 + if show_total { 7 } else { 0 });
    let mut header = format!(
        " {}{}{}{}",
        pad("LINK", name_width),
        pad_left("FR/S", 7),
        pad_left("B/S", 7),
        pad_left("DROP/S", 7),
    );
    if show_total {
        header.push_str(&pad_left("DROPS", 7));
    }
    header.push_str(&pad_left("LAT", 7));
    header.push_str(&format!(" {}", pad("FAULT", fault_width.saturating_sub(1))));
    let mut lines = vec![Line::styled(header, Style::new().fg(FAINT_TEXT))];
    for link in links.into_iter().skip(start).take(rows) {
        let rates = focus.traffic.link(&link.from, &link.to);
        let involved = selected.is_some_and(|s| link.from == s || link.to == s);
        let counters = &link.counters;
        let total = counters.partition_dropped
            + counters.loss_dropped
            + counters.queue_dropped
            + counters.receiver_dropped;
        let dropped = rates.dropped.total();
        let (fault, fault_color) = if let Some(handle) = &link.partitioned_by {
            (format!("✂ {handle}"), RED)
        } else if let Some(handle) = &link.shaped_by {
            let mut text = format!("≈ {handle}");
            if link.policy.loss_ppm > 0 {
                text.push_str(&format!(" {:.1}%", link.policy.loss_ppm as f64 / 1e4));
            }
            (text, YELLOW)
        } else {
            (
                rates.dropped.dominant().map_or("", drop_cause).to_owned(),
                RED_SOFT,
            )
        };
        let mut spans = vec![
            Span::styled(
                format!(
                    " {}",
                    pad(&format!("{}→{}", link.from, link.to), name_width)
                ),
                Style::new().fg(if involved { WHITE } else { TEXT }),
            ),
            Span::styled(
                pad_left(&rate(rates.frames), 7),
                Style::new().fg(if rates.frames >= 0.5 { ICE } else { DIM }),
            ),
            Span::styled(pad_left(&bytes_rate(rates.bytes), 7), Style::new().fg(DIM)),
            Span::styled(
                pad_left(&rate(dropped), 7),
                Style::new().fg(if dropped >= 0.5 { RED } else { DIM }),
            ),
        ];
        if show_total {
            spans.push(Span::styled(
                pad_left(&count(total), 7),
                Style::new().fg(if total > 0 { RED_SOFT } else { DIM }),
            ));
        }
        spans.push(Span::styled(
            pad_left(&latency(link.policy.latency_ns), 7),
            Style::new().fg(if link.shaped_by.is_some() {
                YELLOW
            } else {
                DIM
            }),
        ));
        spans.push(Span::styled(
            format!(" {}", pad(&fault, fault_width.saturating_sub(1))),
            Style::new().fg(fault_color),
        ));
        let mut line = Line::from(spans);
        if involved {
            line = line.style(Style::new().bg(SELECTED));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn when(item: &Item, now: u64) -> (String, Color) {
    match item.state {
        State::Active | State::Down if item.end_ns.is_none() => (
            format!(
                "{} → {}",
                short_time(item.start_ns),
                duration(now.saturating_sub(item.start_ns))
            ),
            span_color(&item.action, true),
        ),
        State::Upcoming => (
            format!(
                "{} · {}",
                short_time(item.start_ns),
                until(item.start_ns, now)
            ),
            ICE,
        ),
        State::Active | State::Closed | State::Down => (
            format!(
                "{} → {}{}",
                short_time(item.start_ns),
                short_time(item.end_ns.unwrap_or(now)),
                item.closed_by
                    .as_ref()
                    .map(|by| format!(" {by}"))
                    .unwrap_or_default()
            ),
            DIM,
        ),
        State::Failed => (format!("{} failed", short_time(item.start_ns)), RED),
        State::Instant => (short_time(item.start_ns), DIM),
    }
}

fn faults(
    frame: &mut Frame,
    app: &App,
    focus: &Focus,
    activity: &Activity,
    area: Rect,
    now: u64,
    tabbed: Option<Panel>,
) {
    let focused = app.panel == Panel::Faults;
    let summary = Line::from(vec![
        Span::styled(
            format!(" {} active", activity.active.len()),
            Style::new().fg(if activity.active.is_empty() { DIM } else { RED }),
        ),
        Span::styled(" · ", Style::new().fg(DIM)),
        Span::styled(
            format!("{} upcoming ", activity.upcoming.len()),
            Style::new().fg(if activity.upcoming.is_empty() {
                DIM
            } else {
                ICE
            }),
        ),
    ])
    .right_aligned();
    let mut b = match tabbed {
        Some(panel) => Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { BORDER_HI } else { BORDER }))
            .title(tabbed_title(panel)),
        None => block("faults", focused),
    }
    .title_bottom(source("GET world · timeline", app.details));
    if tabbed.is_none() || area.width >= 52 {
        b = b.title(summary);
    }
    let inner = b.inner(area);
    frame.render_widget(b, area);
    app.hitboxes.borrow_mut().push((inner, HitTarget::Faults));
    let width = inner.width as usize;
    if width < 12 {
        return;
    }
    enum Entry<'a> {
        Section(&'static str),
        Row(usize, &'a Item),
        Note(&'static str),
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut index = 0;
    for (title, items, empty) in [
        ("ACTIVE", &activity.active, "nothing active"),
        ("UPCOMING · FROM TIMELINE", &activity.upcoming, ""),
        ("HISTORY", &activity.history, ""),
    ] {
        if items.is_empty() && empty.is_empty() {
            continue;
        }
        entries.push(Entry::Section(title));
        if items.is_empty() {
            entries.push(Entry::Note(empty));
        }
        for item in items {
            entries.push(Entry::Row(index, item));
            index += 1;
        }
    }
    let selected_row = focus
        .fault_selected
        .as_ref()
        .and_then(|key| activity.rows().position(|item| &item.key == key));
    let height = inner.height as usize;
    let selected_entry = entries
        .iter()
        .position(|entry| matches!(entry, Entry::Row(row, _) if Some(*row) == selected_row))
        .unwrap_or(0);
    let start = selected_entry.saturating_sub(height.saturating_sub(2));
    let action_width = 10;
    let when_width = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Row(_, item) => Some(when(item, now).0.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(10)
        .clamp(8, width / 2);
    let target_width = width.saturating_sub(2 + action_width + when_width + 1);
    let mut lines = Vec::new();
    for (offset, entry) in entries.iter().skip(start).take(height).enumerate() {
        match entry {
            Entry::Section(title) => lines.push(Line::styled(
                format!(" {title}"),
                Style::new().fg(FAINT_TEXT).add_modifier(Modifier::BOLD),
            )),
            Entry::Note(text) => lines.push(Line::styled(
                format!("   {text}"),
                Style::new().fg(FAINT_TEXT),
            )),
            Entry::Row(row, item) => {
                let selected = Some(*row) == selected_row;
                let (when_text, when_color) = when(item, now);
                let strong = matches!(item.state, State::Active | State::Down | State::Upcoming)
                    || item.state == State::Failed;
                let glyph_color = match item.state {
                    State::Active | State::Down if tick(500).is_multiple_of(2) => {
                        span_color(&item.action, false)
                    }
                    State::Upcoming => ICE,
                    State::Down => RED,
                    State::Failed => RED,
                    _ => span_color(&item.action, strong),
                };
                let text_color = if strong { TEXT } else { DIM };
                let mut line = Line::from(vec![
                    Span::styled(
                        format!(" {} ", glyph(&item.action, item.state)),
                        Style::new().fg(glyph_color).bold(),
                    ),
                    Span::styled(
                        pad(&item.action, action_width),
                        Style::new().fg(if strong { WHITE } else { DIM }),
                    ),
                    Span::styled(pad(&item.target, target_width), Style::new().fg(text_color)),
                    Span::styled(
                        pad_left(&when_text, when_width),
                        Style::new().fg(when_color),
                    ),
                ]);
                if selected {
                    line = line.style(Style::new().bg(SELECTED));
                }
                app.hitboxes.borrow_mut().push((
                    Rect::new(inner.x, inner.y + offset as u16, inner.width, 1),
                    HitTarget::Fault(*row),
                ));
                lines.push(line);
            }
        }
    }
    if entries.len() <= 2 && activity.active.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "   : opens the inject palette",
            Style::new().fg(FAINT_TEXT),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn logs(frame: &mut Frame, app: &App, focus: &Focus, area: Rect) {
    let focused = app.panel == Panel::Logs;
    let filter = focus
        .selected
        .as_deref()
        .map_or_else(|| "all machines".to_string(), str::to_owned);
    let scroll = app.logs_scroll;
    let follow = if scroll == 0 {
        Span::styled("↓ following ", Style::new().fg(GREEN))
    } else {
        Span::styled(
            format!("↑ {scroll} rows back · End follows "),
            Style::new().fg(AMBER),
        )
    };
    let mut title = vec![Span::styled(
        " logs ",
        Style::new()
            .fg(if focused { ICE_BRIGHT } else { ICE })
            .bold(),
    )];
    if app.details {
        title.push(Span::styled("│", Style::new().fg(BORDER)));
        title.push(Span::styled(" events (e) ", Style::new().fg(FAINT_TEXT)));
    }
    let b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { BORDER_HI } else { BORDER }))
        .title(Line::from(title))
        .title(
            Line::from(vec![
                Span::styled(format!(" {filter} · "), Style::new().fg(DIM)),
                follow,
            ])
            .right_aligned(),
        );
    let height = area.height.saturating_sub(2) as usize;
    let excluded = app.logs_exclude.borrow();
    let scope = excluded
        .as_deref()
        .map_or(LogScope::Default, LogScope::Except);
    let rows = log_rows(focus, area.width.saturating_sub(2), scope);
    let end = rows
        .len()
        .saturating_sub(scroll.min(rows.len().saturating_sub(height)));
    let start = end.saturating_sub(height);
    let lines = if rows.is_empty() {
        vec![Line::styled("no output yet", Style::new().fg(DIM))]
    } else {
        rows[start..end].to_vec()
    };
    frame.render_widget(Paragraph::new(lines).block(b), area);
}

/// The oracle's own output with its state and the simulation's verdict.
/// It scrolls like the log pane: rows back from the newest, 0 following.
fn oracle_logs(frame: &mut Frame, app: &App, focus: &Focus, name: &str, area: Rect) {
    let focused = app.panel == Panel::Oracle;
    app.hitboxes.borrow_mut().push((area, HitTarget::Oracle));
    let height = area.height.saturating_sub(2) as usize;
    let rows = log_rows(focus, area.width.saturating_sub(2), LogScope::Only(name));
    let scroll = app.oracle_scroll.min(rows.len().saturating_sub(height));
    let mut right = Vec::new();
    if let Some(m) = focus
        .world
        .as_ref()
        .and_then(|w| w.machines.iter().find(|m| m.name == name))
    {
        let (state, color) = condition(focus, m);
        right.push(Span::styled(format!(" {state} "), Style::new().fg(color)));
    }
    if let Some(resource) = &focus.resource {
        let (text, color) = verdict(resource.status.verdict());
        right.push(Span::styled("· ", Style::new().fg(DIM)));
        right.push(Span::styled(
            format!("verdict {text} "),
            Style::new().fg(color),
        ));
        // Say why a passing oracle's verdict changed.
        if let Some(properties) = &resource.status.properties {
            let reason = if properties.violated > 0 {
                Some(format!("{} violated properties ", properties.violated))
            } else if !properties.evidence_complete {
                Some("property evidence lost ".into())
            } else {
                None
            };
            if let Some(reason) = reason {
                right.push(Span::styled("· ", Style::new().fg(DIM)));
                right.push(Span::styled(reason, Style::new().fg(color)));
            }
        }
    }
    let b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused {
            BORDER_HI
        } else {
            Color::Rgb(91, 63, 122)
        }))
        .title(Line::from(vec![
            Span::styled(
                format!(" {name} "),
                Style::new().fg(machine_color(name)).bold(),
            ),
            Span::styled("◆ ", Style::new().fg(MAGENTA)),
        ]))
        .title(Line::from(right).right_aligned());
    // The top border is full with the name, state and verdict.
    let b = if scroll > 0 {
        b.title_bottom(
            Line::styled(
                format!(" ↑ {scroll} rows back · End follows "),
                Style::new().fg(AMBER),
            )
            .right_aligned(),
        )
    } else {
        b
    };
    let lines = if rows.is_empty() {
        vec![Line::styled("no oracle output yet", Style::new().fg(DIM))]
    } else {
        let end = rows.len() - scroll;
        rows[end.saturating_sub(height)..end].to_vec()
    };
    frame.render_widget(Paragraph::new(lines).block(b), area);
}

/// Which machines' output a log pane shows. The selected machine, when there
/// is one, always narrows the default scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogScope<'a> {
    Default,
    /// Everything but one machine, whose output has its own panel.
    Except(&'a str),
    Only(&'a str),
}

/// Wrapped terminal rows are also the unit of scrolling and viewport anchoring.
/// Application output is shown as written; fault markers come from the world.
pub(super) fn log_rows(focus: &Focus, width: u16, scope: LogScope) -> Vec<Line<'static>> {
    let selected = match scope {
        LogScope::Only(name) => Some(name),
        _ => focus.selected.as_deref(),
    };
    let excluded = match scope {
        LogScope::Except(name) => Some(name),
        _ => None,
    };
    let width = usize::from(width.max(1));
    let mut entries: Vec<(u64, u8, Line<'static>)> = Vec::new();
    for (name, log) in focus.logs.iter().filter(|(name, _)| {
        selected.is_none_or(|selected| selected == name.as_str()) && excluded != Some(name.as_str())
    }) {
        let color = machine_color(name);
        for line in &log.lines {
            let mut spans = vec![Span::styled(
                format!("{} ", short_time(line.time_ns)),
                Style::new().fg(FAINT_TEXT),
            )];
            if selected.is_none() {
                spans.push(Span::styled(format!("{name} "), Style::new().fg(color)));
            }
            if line.stream == LogStream::Stderr {
                spans.push(Span::styled("▍", Style::new().fg(dim(color))));
            } else {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(line.text.clone(), Style::new().fg(TEXT)));
            entries.push((line.time_ns, 1, Line::from(spans)));
        }
    }
    if let Some(world) = &focus.world {
        for fault in &world.faults {
            let (Some(at), FaultStatus::Applied) = (fault.applied_time_ns, fault.status) else {
                continue;
            };
            let target = fault
                .plan
                .as_ref()
                .map(crate::activity::fault_target)
                .or_else(|| fault.machine.clone())
                .unwrap_or_default();
            let relevant = selected.is_none_or(|machine| {
                fault.machine.as_deref() == Some(machine)
                    || target.split(['┃', '→', ' ']).any(|part| part == machine)
            });
            if !relevant {
                continue;
            }
            let label = format!(
                "━━ {} {} {target} ",
                glyph(&fault.action, State::Instant),
                fault.action
            );
            let fill = width.saturating_sub(label.chars().count() + short_time(at).len() + 1);
            entries.push((
                at,
                0,
                Line::from(vec![
                    Span::styled(format!("{} ", short_time(at)), Style::new().fg(FAINT_TEXT)),
                    Span::styled(
                        format!("{label}{}", "━".repeat(fill)),
                        Style::new().fg(dim(span_color(&fault.action, true))),
                    ),
                ]),
            ));
        }
    }
    entries.sort_by_key(|(time, order, _)| (*time, *order));
    let mut rows = Vec::new();
    for (_, _, line) in entries {
        let mut spans = Vec::new();
        let mut used = 0;
        for span in line.spans {
            let mut text = String::new();
            for c in span.content.chars() {
                let mut buffer = [0; 4];
                let cells = Span::raw(&*c.encode_utf8(&mut buffer)).width();
                if used + cells > width && used > 0 {
                    spans.push(Span::styled(std::mem::take(&mut text), span.style));
                    rows.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                }
                text.push(c);
                used += cells;
            }
            spans.push(Span::styled(text, span.style));
        }
        rows.push(Line::from(spans));
    }
    rows
}

fn status_bar(
    frame: &mut Frame,
    focus: &Focus,
    activity: &Activity,
    area: Rect,
    now: u64,
    details: bool,
) {
    frame.render_widget(Block::new().style(Style::new().bg(BAR)), area);
    let (state, color) = phase(focus);
    let mut left = vec![
        Span::styled(
            format!(" {} ", state.to_uppercase()),
            Style::new().fg(color).bold(),
        ),
        Span::styled(format!(" t {} ", time(now)), Style::new().fg(WHITE).bold()),
    ];
    if let Some((p, _)) = &focus.playback {
        if !focus.terminal() {
            let requested = if matches!(p.state.rate, PlaybackRate::Paused) {
                format!(" resumes at {}", rate_label(focus.resume_rate))
            } else {
                format!(" {}", rate_label(p.state.rate))
            };
            left.push(Span::styled(requested, Style::new().fg(TEXT)));
            if let Some(achieved) = p.state.achieved_rate {
                if !matches!(p.state.rate, PlaybackRate::Paused) {
                    left.push(Span::styled(
                        format!(
                            " · achieved {:.2}×",
                            achieved.simulated_time_ns as f64 / achieved.wall_time_ns.get() as f64
                        ),
                        Style::new().fg(DIM),
                    ));
                    if let Some(latest) = focus.achieved.back() {
                        left.push(Span::styled(
                            format!(" {}", rate_history(&focus.achieved, 16)),
                            Style::new().fg(ICE_MID),
                        ));
                        left.push(Span::styled(
                            format!(" now {latest:.2}×"),
                            Style::new().fg(DIM),
                        ));
                    }
                }
            }
            if !p.state.application_ready {
                left.push(Span::styled(
                    if startup_paused(focus) {
                        " · startup paused"
                    } else {
                        " · apps starting"
                    },
                    Style::new().fg(YELLOW),
                ));
            }
        }
        if details {
            let position = &p.state.position;
            left.push(Span::styled(
                format!(
                    "  boundary {} · rounds {}",
                    position.boundary_index, position.completed_rounds
                ),
                Style::new().fg(DIM),
            ));
        }
    }
    if let Some(next) = activity.next(now).filter(|_| focus.executing()) {
        left.push(Span::styled("   next ", Style::new().fg(DIM)));
        left.push(Span::styled(
            format!(
                "{} {} {}",
                glyph(&next.action, State::Upcoming),
                next.action,
                next.target
            ),
            Style::new().fg(ICE),
        ));
        left.push(Span::styled(
            format!(" {}", until(next.start_ns, now)),
            Style::new().fg(DIM),
        ));
    }
    if focus.terminal() {
        left.push(Span::styled("   final observation", Style::new().fg(DIM)));
    }
    let right = vec![
        Span::styled(
            format!(
                "◆ {} checkpoint{}  ",
                focus.checkpoints.len(),
                if focus.checkpoints.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            Style::new().fg(TEAL),
        ),
        Span::styled(
            format!("{} active ", activity.active.len()),
            Style::new().fg(if activity.active.is_empty() { DIM } else { RED }),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    if width_of(&right) + 60 < area.width as usize {
        frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), area);
    }
}

// ------------------------------------------------------------------- catalogue

fn age(created_at_unix_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let seconds = now.saturating_sub(created_at_unix_ms) / 1000;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// How a simulation came to be, from its lineage annotations.
fn kind(s: &SimulationSummary) -> (String, Color) {
    let annotations = &s.metadata.annotations;
    match annotations.get("derivation").map(String::as_str) {
        Some("branch") => (
            match annotations
                .get("branch-at-ns")
                .and_then(|ns| ns.parse::<u64>().ok())
            {
                Some(at) => format!("⑂ branch @ {}", short_time(at)),
                None => "⑂ branch".into(),
            },
            TEAL,
        ),
        Some("edited") => ("✎ edited".into(), ICE),
        Some("rerun") => ("↻ rerun".into(), DIM),
        Some(other) => (other.to_owned(), DIM),
        None if s.metadata.checkpoint.is_some() => ("⑂ branch".into(), TEAL),
        None if annotations.contains_key("derived-from") || s.metadata.parent.is_some() => {
            ("↳ derived".into(), DIM)
        }
        None => ("● run".into(), DIM),
    }
}

/// A one-line strip of a run's faults over simulated time, with a legend of
/// the most recent ones beneath.
fn mini_timeline(
    preview: &crate::model::Preview,
    sim_time: u64,
    completed: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let activity =
        crate::activity::derive(preview.world.as_ref(), &preview.timeline, &[], sim_time);
    let mut items: Vec<&Item> = activity.rows().collect();
    items.sort_by_key(|item| item.start_ns);
    // A finished run never reaches what is still scheduled.
    let unreached = |item: &Item| completed && item.state == State::Upcoming;
    if items.is_empty() {
        return vec![Line::styled("no faults", Style::new().fg(FAINT_TEXT))];
    }
    let end = items
        .iter()
        .map(|item| item.end_ns.unwrap_or(item.start_ns))
        .chain(std::iter::once(sim_time))
        .max()
        .unwrap_or(1)
        .max(1) as f64
        * 1.02;
    let width = width.max(8);
    let column = |at: u64| ((at as f64 / end) * (width - 1) as f64).round() as usize;
    let mut cells: Vec<(char, Color)> = vec![('─', FAINT); width];
    for item in &items {
        let from = column(item.start_ns).min(width - 1);
        let color = match item.state {
            _ if unreached(item) => FAINT_TEXT,
            State::Upcoming => ICE,
            State::Down | State::Failed => RED,
            _ => span_color(&item.action, true),
        };
        if item.is_span() {
            let to = column(item.end_ns.unwrap_or(sim_time)).min(width - 1);
            for cell in cells.iter_mut().take(to + 1).skip(from) {
                *cell = ('━', dim(color));
            }
        }
        cells[from] = (glyph(&item.action, item.state), color);
    }
    let now = column(sim_time).min(width - 1);
    if cells[now].0 == '─' {
        cells[now] = ('│', ICE);
    }
    let strip: Vec<Span> = cells
        .into_iter()
        .map(|(c, color)| Span::styled(c.to_string(), Style::new().fg(color)))
        .collect();
    let mut lines = vec![Line::from(strip)];
    for item in items.iter().take(5) {
        let missed = unreached(item);
        let glyph_color = if missed {
            FAINT_TEXT
        } else if item.state == State::Upcoming {
            ICE
        } else {
            span_color(&item.action, true)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", glyph(&item.action, item.state)),
                Style::new().fg(glyph_color),
            ),
            Span::styled(
                format!("{:<10}", item.action),
                Style::new().fg(if missed { FAINT_TEXT } else { TEXT }),
            ),
            Span::styled(format!("{:<14}", item.target), Style::new().fg(DIM)),
            Span::styled(
                format!(
                    "{}{}",
                    short_time(item.start_ns),
                    if missed { "  not reached" } else { "" }
                ),
                Style::new().fg(FAINT_TEXT),
            ),
        ]));
    }
    if items.len() > 5 {
        lines.push(Line::styled(
            format!("… {} more", items.len() - 5),
            Style::new().fg(FAINT_TEXT),
        ));
    }
    lines
}

/// Faults removed from or added to a run relative to its source, by action
/// and time.
fn changes(
    source: &[seedbox_api::TimelineEntry],
    run: &[seedbox_api::TimelineEntry],
    width: usize,
) -> Vec<Line<'static>> {
    let key = |e: &seedbox_api::TimelineEntry| (e.at_ns, e.action.to_string());
    let line = |sign: &str, color: Color, e: &seedbox_api::TimelineEntry| {
        let text = format!(
            "{sign} {:<10}{:<14}@ {}",
            crate::activity::action_name(&e.action),
            crate::activity::fault_target(&e.action),
            short_time(e.at_ns)
        );
        Line::styled(
            text.chars().take(width).collect::<String>(),
            Style::new().fg(color),
        )
    };
    let mut out: Vec<Line> = Vec::new();
    for entry in source {
        if !run.iter().any(|other| key(other) == key(entry)) {
            out.push(line("−", RED, entry));
        }
    }
    for entry in run {
        if !source.iter().any(|other| key(other) == key(entry)) {
            out.push(line("+", GREEN, entry));
        }
    }
    if out.is_empty() {
        out.push(Line::styled(
            "same faults as its source",
            Style::new().fg(FAINT_TEXT),
        ));
    }
    out
}

/// Whether a listed simulation is running but held at a pause.
fn summary_paused(s: &SimulationSummary) -> bool {
    s.status.phase == Phase::Running
        && s.status
            .playback
            .as_ref()
            .is_some_and(|p| !p.state.terminal && matches!(p.state.rate, PlaybackRate::Paused))
}

fn summary_phase(s: &SimulationSummary) -> (&'static str, Color) {
    match s.status.phase {
        Phase::Queued => ("◌ queued", AMBER),
        Phase::Preparing => ("◌ preparing", YELLOW),
        Phase::Running if s.status.playback.as_ref().is_some_and(|p| p.state.stepping) => {
            ("▷ stepping", YELLOW)
        }
        Phase::Running if summary_paused(s) => ("❚❚ paused", AMBER),
        Phase::Running if s.status.stage == Some(seedbox_api::Stage::Booting) => {
            ("◌ booting", YELLOW)
        }
        Phase::Running => ("● running", GREEN),
        Phase::Finalizing => ("◌ finalizing", YELLOW),
        Phase::Completed => ("■ completed", DIM),
    }
}

fn catalogue(frame: &mut Frame, app: &App, area: Rect) {
    let comparison_height = if app.comparison.is_some() || app.baseline.is_some() {
        7
    } else {
        3
    };
    let [top, bottom] =
        Layout::vertical([Constraint::Min(6), Constraint::Length(comparison_height)]).areas(area);
    let detail_width = (area.width * 36 / 100).clamp(30, 64);
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Min(30), Constraint::Length(detail_width)]).areas(top);
    let tree = app.tree();
    let running = app
        .list
        .iter()
        .filter(|s| s.status.phase == Phase::Running)
        .count();
    let paused = app.list.iter().filter(|s| summary_paused(s)).count();
    let b = block("simulations", true)
        .title(
            Line::styled(
                if paused > 0 {
                    format!(
                        " {} · {} running · {paused} paused ",
                        tree.len(),
                        running - paused
                    )
                } else {
                    format!(" {} · {running} running ", tree.len())
                },
                Style::new().fg(DIM),
            )
            .right_aligned(),
        )
        .title_bottom(source("GET /v1/simulations", app.details));
    let inner = b.inner(list_area);
    frame.render_widget(b, list_area);
    let width = inner.width as usize;
    let show_kind = width >= 70;
    let show_age = width >= 80;
    let id_width = 26;
    let mut header = format!("  {}", pad("ID", id_width));
    if show_kind {
        header.push_str(&pad("KIND", 18));
    }
    header.push_str(&pad("STATE", 13));
    header.push_str(&pad("VERDICT", 11));
    header.push_str(&pad_left("SIM TIME", 11));
    if show_age {
        header.push_str(&pad_left("AGE", 6));
    }
    let mut lines = vec![Line::styled(header, Style::new().fg(FAINT_TEXT))];
    let visible = inner.height.saturating_sub(1) as usize;
    let start = app.list_selected.saturating_sub(visible.saturating_sub(3));
    let mut group: Option<&str> = None;
    for (row, (depth, s)) in tree.iter().enumerate().skip(start) {
        // Top-level runs are grouped under their project's name.
        let name = if s.metadata.name.is_empty() {
            "unnamed"
        } else {
            s.metadata.name.as_str()
        };
        if *depth == 0 && group != Some(name) {
            if group.is_some() {
                lines.push(Line::raw(""));
            }
            group = Some(name);
            let runs = app
                .list
                .iter()
                .filter(|other| other.metadata.name == s.metadata.name)
                .count();
            lines.push(Line::from(vec![
                Span::styled("▍", Style::new().fg(ICE)),
                Span::styled(format!("{name}  "), Style::new().fg(WHITE).bold()),
                Span::styled(
                    format!("{runs} run{}", if runs == 1 { "" } else { "s" }),
                    Style::new().fg(FAINT_TEXT),
                ),
            ]));
        }
        if lines.len() >= inner.height as usize {
            break;
        }
        let selected = row == app.list_selected;
        let marked = app.baseline.as_ref() == Some(&s.metadata.id);
        let prefix = if *depth > 0 {
            format!("{}└ ", "  ".repeat(depth - 1))
        } else {
            String::new()
        };
        let (phase, phase_color) = summary_phase(s);
        let (v, verdict_color) = verdict(s.status.verdict());
        let (kind_text, kind_color) = kind(s);
        let mut spans = vec![
            Span::styled(
                if selected {
                    "▸ "
                } else if marked {
                    "◆ "
                } else {
                    "  "
                },
                Style::new().fg(if marked { TEAL } else { ICE }),
            ),
            Span::styled(prefix.clone(), Style::new().fg(FAINT)),
            Span::styled(
                pad(
                    &s.metadata.id.to_string(),
                    id_width.saturating_sub(prefix.chars().count()),
                ),
                Style::new()
                    .fg(if selected { WHITE } else { TEXT })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        if show_kind {
            spans.push(Span::styled(
                pad(&kind_text, 18),
                Style::new().fg(kind_color),
            ));
        }
        spans.push(Span::styled(pad(phase, 13), Style::new().fg(phase_color)));
        spans.push(Span::styled(pad(v, 11), Style::new().fg(verdict_color)));
        spans.push(Span::styled(
            pad_left(&short_time(s.status.simulated_time_ns), 11),
            Style::new().fg(TEXT),
        ));
        if show_age {
            spans.push(Span::styled(
                pad_left(&age(s.metadata.created_at_unix_ms), 6),
                Style::new().fg(DIM),
            ));
        }
        let y = inner.y + lines.len() as u16;
        app.hitboxes.borrow_mut().push((
            Rect::new(inner.x, y, inner.width, 1),
            HitTarget::Catalogue(row),
        ));
        let mut line = Line::from(spans);
        if selected {
            line = line.style(Style::new().bg(SELECTED));
        }
        lines.push(line);
        if s.status.phase == Phase::Queued && selected {
            lines.push(Line::styled(
                format!("      waiting to start · {running} running on this server"),
                Style::new().fg(FAINT_TEXT),
            ));
        }
    }
    if tree.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            if app.connected {
                "  no simulations yet; start one with `seedbox run --detach`"
            } else {
                "  the local server is unreachable; start it with `seedbox serve`"
            },
            Style::new().fg(DIM),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
    detail(frame, app, detail_area);
    comparison(frame, app, bottom);
}

fn detail(frame: &mut Frame, app: &App, area: Rect) {
    let tree = app.tree();
    let Some((_, s)) = tree.get(app.list_selected) else {
        frame.render_widget(block("detail", false), area);
        return;
    };
    let b = block(s.metadata.id.as_str(), false)
        .title(Line::styled(" ⏎ open ", Style::new().fg(DIM)).right_aligned())
        .title_bottom(source("GET /v1/simulations", app.details));
    let (phase, phase_color) = summary_phase(s);
    let (v, verdict_color) = verdict(s.status.verdict());
    let name = if s.metadata.name.is_empty() {
        s.metadata.id.to_string()
    } else {
        s.metadata.name.clone()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{name}  "), Style::new().fg(WHITE).bold()),
        Span::styled(format!("{phase}  "), Style::new().fg(phase_color)),
        Span::styled(v, Style::new().fg(verdict_color)),
    ])];
    // Lineage, oldest first.
    let mut chain = vec![s.metadata.id.clone()];
    let mut cursor = crate::model::source_of(s);
    while let Some(id) = cursor {
        if chain.contains(&id) || chain.len() > 12 {
            break;
        }
        chain.insert(0, id.clone());
        cursor = app
            .list
            .iter()
            .find(|candidate| candidate.metadata.id == id)
            .and_then(crate::model::source_of);
    }
    if chain.len() > 1 {
        let mut spans = Vec::new();
        for (i, id) in chain.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" ▸ ", Style::new().fg(FAINT)));
            }
            spans.push(Span::styled(
                id.to_string(),
                Style::new().fg(if i + 1 == chain.len() { WHITE } else { ICE }),
            ));
        }
        lines.push(Line::from(spans));
    }
    let (kind_text, kind_color) = kind(s);
    let kv = |k: &str, v: String, color: Color| {
        Line::from(vec![
            Span::styled(format!("{k:<12}"), Style::new().fg(DIM)),
            Span::styled(v, Style::new().fg(color)),
        ])
    };
    lines.push(Line::raw(""));
    lines.push(kv("kind", kind_text, kind_color));
    lines.push(kv("sim time", time(s.status.simulated_time_ns), TEXT));
    lines.push(kv(
        "created",
        format!("{} ago", age(s.metadata.created_at_unix_ms)),
        TEXT,
    ));
    if let Some(termination) = &s.status.termination {
        use seedbox_api::ExecutionTermination as End;
        let (text, color) = match termination {
            End::Error { message } => (format!("error: {message}"), RED),
            End::OracleExited => ("oracle exited".into(), TEXT),
            End::Stopped => ("stopped".into(), TEXT),
            End::SimulatedTimeLimit => ("simulated time limit".into(), TEXT),
            End::WallTimeLimit => ("wall time limit".into(), AMBER),
            End::PreparationTimeLimit => ("preparation time limit".into(), AMBER),
            End::Quiescent => ("quiescent".into(), TEXT),
            End::Interrupted => ("interrupted".into(), AMBER),
        };
        lines.push(kv("ended", text, color));
    }
    if let Some(code) = s.status.oracle_exit_code {
        lines.push(kv(
            "oracle exit",
            code.to_string(),
            if code == 0 { GREEN } else { RED },
        ));
    }
    if s.status.phase == Phase::Queued {
        let running = app
            .list
            .iter()
            .filter(|other| other.status.phase == Phase::Running)
            .count();
        lines.push(kv(
            "waiting",
            format!("{running} running on this server"),
            AMBER,
        ));
    }
    let inner_width = area.width.saturating_sub(2) as usize;
    if let Some(preview) = app.previews.get(&s.metadata.id) {
        lines.push(Line::raw(""));
        lines.push(Line::styled("FAULTS", Style::new().fg(FAINT_TEXT).bold()));
        lines.extend(mini_timeline(
            preview,
            s.status.simulated_time_ns,
            s.status.phase == Phase::Completed,
            inner_width,
        ));
        let source = crate::model::source_of(s).and_then(|id| app.previews.get(&id));
        let derivation = s.metadata.annotations.get("derivation").map(String::as_str);
        if let (Some(source), false) = (source, derivation == Some("branch")) {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "CHANGES FROM SOURCE",
                Style::new().fg(FAINT_TEXT).bold(),
            ));
            lines.extend(changes(&source.timeline, &preview.timeline, inner_width));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("STAGES", Style::new().fg(FAINT_TEXT).bold()));
    let longest = s
        .status
        .stage_timings
        .values()
        .map(|t| t.elapsed_wall_ns)
        .max()
        .unwrap_or(1)
        .max(1);
    let bar_width = (area.width as usize).saturating_sub(26).clamp(4, 24);
    for (stage, timing) in &s.status.stage_timings {
        let width =
            (timing.elapsed_wall_ns as f64 / longest as f64 * bar_width as f64).round() as usize;
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12}", format!("{stage:?}").to_lowercase()),
                Style::new().fg(DIM),
            ),
            Span::styled(
                "▇".repeat(width.max(1)),
                Style::new().fg(if timing.complete { ICE_DIM } else { ICE }),
            ),
            Span::styled(
                format!(" {:.1}s", timing.elapsed_wall_ns as f64 / 1e9),
                Style::new().fg(DIM),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(b).wrap(Wrap { trim: false }),
        area,
    );
}

fn control_summary(control: &serde_json::Value) -> (String, Color) {
    let command = &control["request"]["command"];
    let state = &control["state"];
    let (action, acknowledged) = match command["type"].as_str().unwrap_or("Control") {
        "setRate" => match command["rate"]["mode"].as_str() {
            Some("paused") => ("Pause".into(), "Paused".into()),
            Some("unlimited") => ("Resume".into(), "Resumed · unlimited".into()),
            _ => {
                let rate = serde_json::from_value::<PlaybackRate>(command["rate"].clone())
                    .map(rate_label)
                    .unwrap_or_else(|_| "updated rate".into());
                (format!("Set rate {rate}"), format!("Playback rate {rate}"))
            }
        },
        "step" => ("Step".into(), "Step admitted".into()),
        "checkpoint" => ("Checkpoint".into(), "Checkpoint acknowledged".into()),
        "pauseBeforeNextFault" => (
            "Pause before next fault".into(),
            "Running to next fault".into(),
        ),
        "injectFault" => {
            let fault = &command["action"];
            let name = fault["action"].as_str().unwrap_or("fault");
            let target = fault["machine"]
                .as_str()
                .or_else(|| fault["handle"].as_str());
            let label =
                target.map_or_else(|| name.to_owned(), |target| format!("{name}: {target}"));
            (format!("Inject {label}"), format!("Admitted {label}"))
        }
        name => (name.to_owned(), format!("{name} acknowledged")),
    };
    match state["type"].as_str() {
        Some("accepted") => (format!("{action} requested"), DIM),
        Some("acknowledged") if !state["error"].is_null() => (format!("{action} rejected"), RED),
        Some("acknowledged") => (acknowledged, ICE),
        Some("unsupported") => (format!("{action} unsupported"), RED),
        Some("interrupted") => (format!("{action} interrupted · outcome unknown"), ORANGE),
        state => (
            format!("{action} · {}", state.unwrap_or("unknown status")),
            DIM,
        ),
    }
}

fn event_text(kind: &EventKind) -> (String, String, Color) {
    match kind {
        EventKind::OverrideRejected { reason, .. } => {
            ("fault".into(), format!("Override rejected: {reason}"), RED)
        }
        EventKind::FaultChanged {
            action,
            status,
            plan,
            machine,
            ..
        } => {
            let target = plan
                .as_deref()
                .map(crate::activity::fault_target)
                .or_else(|| machine.clone())
                .unwrap_or_default();
            (
                "fault".into(),
                format!(
                    "{action} {}{}",
                    format!("{status:?}").to_lowercase(),
                    if target.is_empty() {
                        String::new()
                    } else {
                        format!(": {target}")
                    }
                ),
                match status {
                    FaultStatus::Applied => YELLOW,
                    FaultStatus::Failed | FaultStatus::Unavailable => RED,
                    _ => DIM,
                },
            )
        }
        EventKind::MachineLifecycle {
            machine,
            incarnation,
            transition,
        } => (
            "machine".into(),
            format!("{machine} i{incarnation} {transition:?}").to_lowercase(),
            ORANGE,
        ),
        EventKind::MachineReady {
            machine,
            incarnation,
        } => (
            "machine".into(),
            format!("{machine} i{incarnation} ready"),
            GREEN,
        ),
        EventKind::MachineExited { machine, code, .. } => (
            "machine".into(),
            format!("{machine} exited {code}"),
            if *code == 0 { DIM } else { RED },
        ),
        EventKind::ControlChanged { control } => {
            let (text, color) = control_summary(control);
            ("control".into(), text, color)
        }
        EventKind::LifecycleChanged { phase, .. } => (
            "lifecycle".into(),
            format!("{phase:?}").to_lowercase(),
            TEXT,
        ),
        EventKind::OracleExited { code } => (
            "oracle".into(),
            format!("exited {code}"),
            if *code == 0 { GREEN } else { RED },
        ),
        EventKind::PreparationProgress { machine, stage } => (
            "prepare".into(),
            format!("{machine} {}", format!("{stage:?}").to_lowercase()),
            DIM,
        ),
        EventKind::ExecutionTerminated { termination } => (
            "terminated".into(),
            format!("{termination:?}").to_lowercase(),
            DIM,
        ),
        other => (
            "event".into(),
            serde_json::to_value(other)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
                .unwrap_or_default(),
            DIM,
        ),
    }
}

/// The fault a control admitted, if it admitted one.
fn admitted_fault(kind: &EventKind) -> Option<u64> {
    let EventKind::ControlChanged { control } = kind else {
        return None;
    };
    control["state"]["intervention"]["plan"]["id"].as_u64()
}

/// The server can report a fault as applied before the control that admitted
/// it. Show the admission first when both happened at the same simulated time.
fn admissions_first(events: &mut Vec<&seedbox_api::SimulationEvent>) {
    let mut index = 0;
    while index < events.len() {
        let moved = admitted_fault(&events[index].event).and_then(|id| {
            events[..index].iter().position(|earlier| {
                earlier.simulated_time_ns == events[index].simulated_time_ns
                    && matches!(&earlier.event, EventKind::FaultChanged { id: fault, .. } if u64::from(*fault) == id)
            })
        });
        if let Some(to) = moved {
            let admission = events.remove(index);
            events.insert(to, admission);
        }
        index += 1;
    }
}

fn events(
    frame: &mut Frame,
    focus: &Focus,
    area: Rect,
    details: bool,
    scroll: usize,
    focused: bool,
) {
    let mut b = block("events", focused).title_bottom(source("GET events (SSE)", details));
    if focus.lost > 0 {
        b = b.title(
            Line::styled(format!(" {} lost ", focus.lost), Style::new().fg(ORANGE)).right_aligned(),
        );
    }
    let height = area.height.saturating_sub(2) as usize;
    let mut shown_controls = std::collections::BTreeSet::new();
    let visible: Vec<_> = focus
        .events
        .iter()
        .rev()
        .filter(|event| {
            if !details {
                if let EventKind::ControlChanged { control } = &event.event {
                    if let Some(id) = control["request"]["requestId"].as_str() {
                        return shown_controls.insert(id);
                    }
                }
            }
            true
        })
        .skip(scroll.min(focus.events.len().saturating_sub(height)))
        .take(height)
        .collect();
    let mut visible: Vec<_> = visible.into_iter().rev().collect();
    admissions_first(&mut visible);
    let stamp = |e: &seedbox_api::SimulationEvent| {
        e.simulated_time_ns.map(time).unwrap_or_else(|| "—".into())
    };
    let stamp_width = visible
        .iter()
        .map(|e| stamp(e).chars().count())
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = visible
        .into_iter()
        .map(|e| {
            let (kind, mut text, color) = event_text(&e.event);
            if details {
                if let EventKind::FaultChanged { id, .. } = &e.event {
                    text.push_str(&format!(" · #{id}"));
                }
                if let EventKind::ControlChanged { control } = &e.event {
                    if let Some(id) = control["request"]["requestId"].as_str() {
                        text.push_str(&format!(" · {id}"));
                    }
                }
            }
            Line::from(vec![
                Span::styled(pad_left(&stamp(e), stamp_width), Style::new().fg(DIM)),
                Span::styled(format!(" {kind:<9} "), Style::new().fg(ICE_DIM)),
                Span::styled(text, Style::new().fg(color)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(b), area);
}

fn comparison(frame: &mut Frame, app: &App, area: Rect) {
    let b = block("compare", false).title_bottom(source("GET compare/{other}", app.details));
    let Some(c) = &app.comparison else {
        let hint = match &app.baseline {
            Some(id) => format!("baseline {id}; select another simulation and press c"),
            None => "press m on a simulation to mark it as the baseline, then c on another".into(),
        };
        frame.render_widget(Paragraph::new(hint).fg(DIM).block(b), area);
        return;
    };
    let mark = |v: Option<bool>| match v {
        Some(true) => ("match", GREEN),
        Some(false) => ("differs", RED),
        None => ("n/a", DIM),
    };
    let status_color = match c.status {
        seedbox_api::ComparisonStatus::Match => GREEN,
        seedbox_api::ComparisonStatus::Mismatch => RED,
        seedbox_api::ComparisonStatus::NotComparable => YELLOW,
    };
    let row = |k: &str, v: Option<bool>| {
        let (t, color) = mark(v);
        Line::from(vec![
            Span::styled(format!("{k:<16}"), Style::new().fg(DIM)),
            Span::styled(t, Style::new().fg(color).bold()),
        ])
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ↔ {}  ", c.baseline, c.simulation),
                Style::new().fg(TEXT),
            ),
            Span::styled(
                format!("{:?}", c.status).to_uppercase(),
                Style::new().fg(status_color).bold(),
            ),
        ]),
        row("outcome", Some(c.outcome_matches)),
        row("witness", c.witness_matches),
        row("full state", c.full_state_matches),
        row("fault history", Some(c.fault_history_matches)),
    ];
    frame.render_widget(Paragraph::new(lines).block(b), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{app, world};
    use crate::model::Update;
    use ratatui::{backend::TestBackend, Terminal};
    use seedbox_api::{
        EventDelivery, FaultState, Frame as WireFrame, FrameDelivery, LogDelivery, LogEntry,
        SimulationEvent,
    };

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(160, 48)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        if std::env::var_os("SEEDBOX_TUI_DUMP").is_some() {
            println!("{text}");
        }
        text
    }

    #[test]
    fn mouse_uses_rendered_panel_and_tab_hitboxes() {
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut app = app(&["a", "oracle"]);
        app.apply(Update::Log(
            "k7m2p9qx".parse().unwrap(),
            seedbox_api::LogDelivery::Entry {
                sequence: 1,
                entry: seedbox_api::LogEntry::Bytes {
                    machine: "a".into(),
                    incarnation: 0,
                    simulated_time_ns: 0,
                    stream: LogStream::Stdout,
                    bytes: b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n".to_vec(),
                },
            },
        ));
        render(&app);
        let rect = |app: &App, target: HitTarget| {
            app.hitboxes
                .borrow()
                .iter()
                .find(|(_, candidate)| *candidate == target)
                .unwrap()
                .0
        };
        let logs = rect(&app, HitTarget::Logs);
        assert_eq!(logs.x, 0);
        assert_eq!(logs.width, 160);
        let mouse = |rect: Rect, kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: rect.x + 1,
                row: rect.y + 1,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.input(mouse(logs, MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(app.panel, Panel::Logs);
        app.input(mouse(logs, MouseEventKind::ScrollUp));
        assert!(app.logs_scroll > 0);
        let machine = rect(&app, HitTarget::Machine(1));
        app.input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: machine.x,
            row: machine.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(
            app.focus.as_ref().unwrap().selected.as_deref(),
            Some("oracle")
        );
        let tab = rect(&app, HitTarget::Screen(Screen::Timeline));
        app.input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: tab.x,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.screen, Screen::Timeline);
        assert_eq!(app.fault_menu, None);
    }

    #[test]
    fn every_machine_shows_its_configured_or_throttled_speed() {
        assert_eq!(frequency(2_000_000_000), "2GHz");
        assert_eq!(frequency(1_500_000_000), "1.5GHz");
        assert_eq!(frequency(333_000_000), "333MHz");
        assert_eq!(frequency(333_333_333), "333.34MHz");
        assert_eq!(frequency(800_000), "800kHz");
        assert_eq!(frequency(999), "999Hz");
        let mut app = app(&["app", "oracle"]);
        let text = render(&app);
        assert!(text.matches("● ready 1GHz").count() >= 2, "{text}");
        let mut baseline = world(&["app", "oracle"]);
        baseline.machines[0].cpu_speed_hz = Some(2_000_000_000);
        app.apply(Update::World(baseline.clone()));
        let text = render(&app);
        assert!(text.contains("● ready 2GHz"), "{text}");
        assert!(text.contains("● ready 1GHz"), "{text}");
        // Observations carry the effective speed, including a saturated
        // throttle: a 2 GHz CPU requested at 1 kHz actually runs at 2 MHz.
        for (effective, label) in [(500_000_000, "500MHz"), (2_000_000, "2MHz")] {
            let mut w = baseline.clone();
            w.machines[0].throttle_speed_hz = Some(effective);
            app.apply(Update::World(w));
            let text = render(&app);
            assert!(text.contains(&format!("● ready {label}")), "{text}");
            assert!(text.contains("● ready 1GHz"), "{text}");
        }
        app.apply(Update::World(baseline.clone()));
        let text = render(&app);
        assert!(text.contains("● ready 2GHz"), "{text}");
        assert!(text.contains("● ready 1GHz"), "{text}");
        assert!(!text.contains("2MHz"), "{text}");
        // The machines table widens its state column instead of cutting a
        // longer state's speed short.
        let mut exited = baseline.clone();
        exited.machines[1].condition = MachineCondition::Exited;
        exited.machines[1].exit_code = Some(0);
        app.apply(Update::World(exited));
        let text = render(&app);
        let row = text
            .lines()
            .find(|line| line.contains("oracle") && line.contains("exited"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(row.contains("exited 0 1GHz "), "{row}");
    }

    #[test]
    fn resolved_names_and_unbounded_playback_render_without_private_defaults() {
        let mut app = app(&["app", "oracle"]);
        let mut resource = crate::model::tests::timeline_resource(10);
        resource.metadata.name = "metadata application".into();
        resource.spec.configuration.name.clear();
        resource.status.phase = Phase::Running;
        resource.status.simulated_time_ns = 2_000_000_000;
        assert_eq!(
            resource.spec.configuration.limits.simulated_time_ns,
            u64::MAX
        );
        app.apply(Update::Resource(resource));
        app.apply(Update::Playback(seedbox_api::PlaybackObservation {
            state: seedbox_api::PlaybackStatus {
                position: Default::default(),
                rate: PlaybackRate::Unlimited,
                achieved_rate: None,
                stepping: false,
                terminal: false,
                application_ready: true,
            },
            age_ns: 0,
        }));
        let text = render(&app);
        assert!(text.contains("network · metadata application"));
        assert!(text
            .lines()
            .next()
            .unwrap()
            .contains("metadata application"));
        assert!(text.contains(&time(2_000_000_000)));
        assert!(text.contains("unlimited"));
        assert!(!text.contains("18446744073"));
    }

    #[test]
    fn admissions_show_before_the_faults_they_admitted() {
        let event = |time, event| SimulationEvent {
            simulation: "k7m2p9qx".parse().unwrap(),
            revision: 1,
            simulated_time_ns: Some(time),
            position: None,
            event,
        };
        let applied = |id| EventKind::FaultChanged {
            id,
            input_key: None,
            action: "heal".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(5),
            plan: None,
        };
        let admitted = |id: u32| EventKind::ControlChanged {
            control: Box::new(serde_json::json!({
                "request": {"command": {"type": "injectFault", "action": {"action": "heal"}}},
                "state": {"type": "acknowledged", "error": null, "intervention": {"plan": {"id": id}}}
            })),
        };
        let earlier = event(1, applied(1));
        let fault = event(5, applied(2));
        let control = event(5, admitted(2));
        let unrelated = event(6, admitted(1));
        let mut events = vec![&earlier, &fault, &control, &unrelated];
        admissions_first(&mut events);
        assert!(std::ptr::eq(events[1], &control));
        assert!(std::ptr::eq(events[2], &fault));
        // Only an admission at the same simulated time moves.
        assert!(std::ptr::eq(events[3], &unrelated));
    }

    #[test]
    fn every_link_is_reachable_by_scrolling_at_compact_heights() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let names = ["n1", "n2", "n3", "n4", "oracle"];
        let all: Vec<String> = names
            .iter()
            .flat_map(|a| {
                names
                    .iter()
                    .filter(move |b| *b != a)
                    .map(move |b| format!("{a}→{b}"))
            })
            .collect();
        let press = |app: &mut App, code| {
            app.input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        };
        // The smallest wide layout and a narrow one, where links are a tab.
        for (width, height) in [(130, 36), (100, 30)] {
            let mut app = app(&names);
            app.focus.as_mut().unwrap().selected = None;
            press(&mut app, KeyCode::Tab);
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.panel, Panel::Links);
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..all.len() {
                let text = render_at(&app, width, height);
                for link in &all {
                    if text.contains(link.as_str()) {
                        seen.insert(link.clone());
                    }
                }
                press(&mut app, KeyCode::Down);
            }
            assert_eq!(seen.len(), all.len(), "{width}x{height} missed links");
            // Scrolling stops with the last page full.
            assert_eq!(app.links_scroll, all.len() - app.links_rows.get());
            let text = render_at(&app, width, height);
            assert!(text.contains(&format!(" of {} links", all.len())));
        }
    }

    #[test]
    fn the_oracle_panel_scrolls_back_and_holds_still_while_output_arrives() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = app(&["a", "b", "oracle"]);
        app.focus.as_mut().unwrap().selected = None;
        let line = |sequence: u64| {
            Update::Log(
                "k7m2p9qx".parse().unwrap(),
                LogDelivery::Entry {
                    sequence,
                    entry: LogEntry::Bytes {
                        machine: "oracle".into(),
                        incarnation: 0,
                        simulated_time_ns: 0,
                        stream: LogStream::Stdout,
                        bytes: format!("step {sequence:03}\n").into_bytes(),
                    },
                },
            )
        };
        for sequence in 1..=80 {
            app.apply(line(sequence));
        }
        let press = |app: &mut App, code| {
            app.input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        };
        let text = render(&app);
        assert!(text.contains("step 080"));
        // Tab reaches the oracle panel once it is shown.
        while app.panel != Panel::Oracle {
            press(&mut app, KeyCode::Tab);
        }
        press(&mut app, KeyCode::PageUp);
        press(&mut app, KeyCode::PageUp);
        press(&mut app, KeyCode::PageUp);
        press(&mut app, KeyCode::PageUp);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.oracle_scroll, 41);
        // The application log pane did not move.
        assert_eq!(app.logs_scroll, 0);
        let text = render(&app);
        assert!(!text.contains("step 080"));
        assert!(text.contains("step 039"));
        assert!(text.contains("41 rows back"));
        // New output does not move the rows being read.
        app.apply(line(81));
        assert_eq!(app.oracle_scroll, 42);
        assert!(render(&app).contains("step 039"));
        // Scrolling stops at the first row; End follows again.
        for _ in 0..20 {
            press(&mut app, KeyCode::PageUp);
        }
        assert!(render(&app).contains("step 001"));
        press(&mut app, KeyCode::End);
        assert_eq!(app.oracle_scroll, 0);
        assert!(render(&app).contains("step 081"));
    }

    #[test]
    fn wide_screens_show_links_and_the_oracle_beside_application_logs() {
        let mut app = app(&["a", "b", "oracle"]);
        app.focus.as_mut().unwrap().selected = None;
        let output = |machine: &str, sequence, bytes: &[u8]| {
            Update::Log(
                "k7m2p9qx".parse().unwrap(),
                LogDelivery::Entry {
                    sequence,
                    entry: LogEntry::Bytes {
                        machine: machine.into(),
                        incarnation: 0,
                        simulated_time_ns: 0,
                        stream: LogStream::Stdout,
                        bytes: bytes.to_vec(),
                    },
                },
            )
        };
        app.apply(output("a", 1, b"application line\n"));
        app.apply(output("oracle", 2, b"workload line\n"));
        let mut cut = world(&["a", "b", "oracle"]);
        cut.revision = 2;
        for link in &mut cut.links {
            if (link.from == "a") != (link.to == "a")
                && link.to != "oracle"
                && link.from != "oracle"
            {
                link.partitioned_by = Some("p1".into());
            }
        }
        app.apply(Update::World(cut));
        let text = render(&app);
        assert!(text.contains("oracle ◆"));
        assert!(text.contains("workload line"));
        assert!(text.contains("application line"));
        assert!(text.contains(" links "));
        assert!(text.contains("a→b"));
        assert!(text.contains("✂ p1"));
        let logs = app
            .hitboxes
            .borrow()
            .iter()
            .find(|(_, target)| *target == HitTarget::Logs)
            .unwrap()
            .0;
        assert!(logs.width < 160);
        // The oracle's output is not repeated in the application log pane.
        let rows = log_rows(
            app.focus.as_ref().unwrap(),
            logs.width,
            LogScope::Except("oracle"),
        );
        assert!(rows.iter().all(|row| !row.to_string().contains("workload")));
        // Selecting a machine returns to one full-width log pane.
        app.focus.as_mut().unwrap().selected = Some("a".into());
        let text = render(&app);
        assert!(!text.contains("workload line"));
    }

    #[test]
    fn operator_events_hide_ids_but_details_restore_them() {
        let mut app = app(&["db"]);
        app.apply(Update::Event(EventDelivery::Event {
            sequence: 1,
            observation: SimulationEvent {
                simulation: "k7m2p9qx".parse().unwrap(), revision: 1,
                simulated_time_ns: Some(100), position: None,
                event: EventKind::ControlChanged { control: Box::new(serde_json::json!({
                    "request":{"requestId":"debug-request-123", "command":{"type":"setRate","rate":{"mode":"paused"}}},
                    "state":{"type":"acknowledged","error":null}
                })) },
            },
        }));
        let focus = app.focus.as_mut().unwrap();
        let mut accepted = focus.events.back().unwrap().clone();
        if let EventKind::ControlChanged { control } = &mut accepted.event {
            control["state"]["type"] = "accepted".into();
        }
        focus.events.push_front(accepted);
        // Raw events live behind the logs panel's events tab.
        assert!(!render(&app).contains("Paused"));
        app.input(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('e'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let normal = render(&app);
        assert!(normal.contains("Paused"));
        assert!(!normal.contains("Pause requested"));
        assert!(!normal.contains("debug-request-123"));
        assert!(!normal.contains("GET world"));
        app.input(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('v'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let details = render(&app);
        assert!(details.contains("debug-request-123"));
        assert!(details.contains("Pause requested"));
        assert!(details.contains("GET world"));
        app.inspector = Some(crate::model::Inspector {
            title: "pause".into(),
            request: "raw-payload-secret-to-operator-view".into(),
            result: "connection lost".into(),
            ok: false,
            at: Instant::now(),
        });
        app.details = false;
        let failed = render(&app);
        assert!(failed.contains("connection lost"));
        assert!(!failed.contains("raw-payload-secret-to-operator-view"));
    }

    #[test]
    fn control_summaries_distinguish_requests_admission_and_uncertainty() {
        let mut receipt = serde_json::json!({
            "request":{"command":{"type":"injectFault","action":{"action":"future_fault","machine":"DB"}}},
            "state":{"type":"accepted"}
        });
        assert_eq!(
            control_summary(&receipt).0,
            "Inject future_fault: DB requested"
        );
        receipt["state"]["type"] = "acknowledged".into();
        assert_eq!(control_summary(&receipt).0, "Admitted future_fault: DB");
        receipt["state"]["error"] = "invalidFault".into();
        assert_eq!(control_summary(&receipt).1, RED);
        receipt["state"]["type"] = "interrupted".into();
        assert!(control_summary(&receipt).0.contains("outcome unknown"));
    }

    #[test]
    fn completed_screen_offers_inspection_and_rerun_without_live_rate() {
        let mut app = app(&["a"]);
        app.apply(Update::Playback(seedbox_api::PlaybackObservation {
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
        let text = render(&app);
        assert!(text.contains("final observation"));
        assert!(text.contains("rerun"));
        assert!(!text.contains("unlimited"));
        assert!(!text.contains("c  checkpoint"));
    }

    #[test]
    fn the_simulation_screen_shows_only_what_the_api_reported() {
        let mut app = app(&["api", "db", "oracle"]);
        let mut w = world(&["api", "db", "oracle"]);
        w.simulated_time_ns = 2_000_000;
        w.faults.push(FaultState {
            input_key: None,
            id: 3,
            action: "partition".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(1_500_000),
            plan: Some(serde_json::json!({"action":"partition","handle":"split","left":["api"],"right":["db"]})),
            active: true,
        });
        for link in &mut w.links {
            if (link.from == "api" && link.to == "db") || (link.from == "db" && link.to == "api") {
                link.partitioned_by = Some("split".into());
            }
        }
        app.apply(Update::World(w));
        app.apply(Update::Frames(FrameDelivery::Frames {
            sequence: 1,
            simulated_time_ns: 2_000_000,
            frames: vec![WireFrame {
                from: "oracle".into(),
                to: "api".into(),
                sent_ns: 1_800_000,
                due_ns: Some(2_800_000),
                bytes: 120,
                fate: FrameFate::Scheduled,
            }],
        }));
        app.apply(Update::Event(EventDelivery::Event {
            sequence: 1,
            observation: SimulationEvent {
                simulation: "k7m2p9qx".parse().unwrap(),
                revision: 1,
                simulated_time_ns: Some(1_000_000),
                position: None,
                event: EventKind::MachineReady {
                    machine: "db".into(),
                    incarnation: 0,
                },
            },
        }));
        app.apply(Update::Log(
            "k7m2p9qx".parse().unwrap(),
            LogDelivery::Entry {
                sequence: 1,
                entry: LogEntry::Bytes {
                    machine: "api".into(),
                    incarnation: 0,
                    simulated_time_ns: 1_200_000,
                    stream: LogStream::Stdout,
                    bytes: b"listening on :8080\n".to_vec(),
                },
            },
        ));
        let screen = render(&app);
        for expected in ["api", "db", "oracle", "split", "listening on :8080"] {
            assert!(screen.contains(expected), "missing {expected}:\n{screen}");
        }
        assert!(!screen.contains("cache"));

        // A machine the server adds later appears without any client change.
        let mut added = world(&["api", "cache", "db", "oracle"]);
        added.revision = 2;
        added.simulated_time_ns = 3_000_000;
        app.apply(Update::World(added));
        let screen = render(&app);
        assert!(screen.contains("cache"), "{screen}");
        let mut removed = world(&["api", "cache", "oracle"]);
        removed.revision = 3;
        removed.simulated_time_ns = 4_000_000;
        app.apply(Update::World(removed));
        // Historical events may still name removed machines; check the current
        // topology without that history in this render assertion.
        app.focus.as_mut().unwrap().events.clear();
        let screen = render(&app);
        assert!(
            !screen.contains("db"),
            "removed machine remains visible: {screen}"
        );
    }
    #[test]
    fn unselected_logs_merge_machines_in_time_order_and_selection_filters() {
        let mut app = app(&["api", "db"]);
        app.focus.as_mut().unwrap().selected = None;
        for (sequence, machine, time, text) in [
            (1, "db", 30, "db-last\n"),
            (2, "api", 10, "api-first\n"),
            (3, "db", 20, "db-middle\n"),
        ] {
            app.apply(Update::Log(
                "k7m2p9qx".parse().unwrap(),
                LogDelivery::Entry {
                    sequence,
                    entry: LogEntry::Bytes {
                        machine: machine.into(),
                        incarnation: 0,
                        simulated_time_ns: time,
                        stream: LogStream::Stdout,
                        bytes: text.as_bytes().to_vec(),
                    },
                },
            ));
        }
        let screen = render(&app);
        assert!(screen.contains("all machines"));
        assert!(screen.contains("api  api-first"));
        assert!(screen.contains("db  db-middle"));
        assert!(screen.find("api-first").unwrap() < screen.find("db-middle").unwrap());
        assert!(screen.find("db-middle").unwrap() < screen.find("db-last").unwrap());
        app.focus.as_mut().unwrap().selected = Some("api".into());
        let screen = render(&app);
        assert!(screen.contains("api-first"));
        assert!(!screen.contains("db-middle"));
        assert!(!screen.contains("db-last"));
    }

    fn render_at(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn losing_the_server_is_announced_instead_of_an_empty_catalogue() {
        let mut app = App::new(None);
        app.apply(Update::Connection(false));
        let text = render_at(&app, 120, 30);
        assert!(text.contains("lost the local server"));
        assert!(text.contains("server unreachable"));
        assert!(!text.contains("no simulations yet"));
        app.apply(Update::Connection(true));
        let text = render_at(&app, 120, 30);
        assert!(!text.contains("lost the local server"));
    }

    #[test]
    fn faults_panel_and_time_bar_show_active_and_upcoming_items_at_every_size() {
        let mut app = app(&["api", "db"]);
        let mut w = world(&["api", "db"]);
        w.revision = 2;
        w.simulated_time_ns = 2_000_000_000;
        w.faults.push(FaultState {
            input_key: None,
            id: 1,
            action: "partition".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(1_000_000_000),
            plan: Some(serde_json::json!({"action":"partition","handle":"p1","left":["api"],"right":["db"]})),
            active: true,
        });
        app.apply(Update::World(w));
        app.focus.as_mut().unwrap().timeline = vec![seedbox_api::TimelineEntry {
            id: "program:heal".into(),
            at_ns: 5_000_000_000,
            action: serde_json::json!({"action":"heal","handle":"p1"}),
            origin: seedbox_api::TimelineOrigin::Program,
            position: None,
            fault_id: None,
        }];
        let wide = render_at(&app, 200, 50);
        for expected in [
            "ACTIVE",
            "UPCOMING",
            "partition",
            "p1 api┃db",
            "heal p1",
            "╭ time",
            "▼",
        ] {
            assert!(wide.contains(expected), "missing {expected}:\n{wide}");
        }
        assert!(wide.contains("next ◇ heal"));
        for (width, height) in [(100, 30), (80, 24)] {
            let narrow = render_at(&app, width, height);
            assert!(narrow.contains("machines"), "{narrow}");
            assert!(narrow.contains("api"), "{narrow}");
            // Node boxes collapse to badges rather than overlapping.
            assert!(!narrow.contains("╭ api"), "{narrow}");
        }
    }

    #[test]
    fn catalogue_shows_paused_runs_as_paused() {
        let summary = |id: &str, rate| {
            let status = seedbox_api::SimulationStatus {
                phase: Phase::Running,
                stage: Some(seedbox_api::Stage::Executing),
                playback: Some(seedbox_api::PlaybackObservation {
                    state: seedbox_api::PlaybackStatus {
                        position: Default::default(),
                        rate,
                        achieved_rate: None,
                        stepping: false,
                        terminal: false,
                        application_ready: true,
                    },
                    age_ns: 0,
                }),
                ..Default::default()
            };
            SimulationSummary {
                metadata: seedbox_api::Metadata {
                    name: "demo".into(),
                    labels: Default::default(),
                    annotations: Default::default(),
                    id: id.parse().unwrap(),
                    revision: 1,
                    created_at_unix_ms: 1,
                    deletion_requested_at_unix_ms: None,
                    parent: None,
                    checkpoint: None,
                },
                status,
                oracle: "oracle".into(),
            }
        };
        let mut app = App::new(None);
        app.apply(Update::List(vec![
            summary("aaaaaaaa", PlaybackRate::Paused),
            summary("bbbbbbbb", PlaybackRate::Unlimited),
        ]));
        let text = render_at(&app, 160, 30);
        assert!(text.contains("❚❚ paused"), "{text}");
        assert!(text.contains("● running"), "{text}");
        assert!(text.contains("1 running · 1 paused"), "{text}");
    }
    #[test]
    fn paused_startup_shows_resume_hint_instead_of_boot_progress() {
        let app = crate::model::tests::booting_app(PlaybackRate::Paused);
        let text = render(&app);
        assert!(text.contains("startup paused"), "{text}");
        assert!(text.contains("Press Space to resume startup"), "{text}");
        assert!(!text.contains("booting"), "{text}");
        assert!(!text.contains("apps starting"), "{text}");
        let running = crate::model::tests::booting_app(PlaybackRate::Unlimited);
        assert!(render(&running).contains("booting machines"));
    }
    #[test]
    fn machine_card_omits_completed_events_and_preserves_active_downtime() {
        let app = app(&["n1"]);
        let focus = app.focus.as_ref().unwrap();
        let machine = &focus.world.as_ref().unwrap().machines[0];
        for (action, recovery, expected) in [
            ("crashed", "reboot", "rebooted"),
            ("paused", "resume", "resumed"),
            ("exited 1", "ready", "exited"),
        ] {
            let mut item = Item {
                key: crate::activity::Key::Transition(0),
                lane: crate::activity::Lane::Machine("n1".into()),
                action: action.into(),
                target: "n1".into(),
                handle: None,
                start_ns: 3_072_000_000,
                end_ns: Some(3_088_000_000),
                closed_by: Some(recovery.into()),
                state: State::Down,
            };
            let mut terminal = Terminal::new(TestBackend::new(32, 7)).unwrap();
            terminal
                .draw(|frame| {
                    node(
                        frame,
                        focus,
                        machine,
                        frame.area(),
                        &[&item],
                        false,
                        3_306_000_000,
                    )
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(!text.contains(action), "{text}");
            assert!(!text.contains(expected), "{text}");
            assert!(!text.contains("ago"), "{text}");
            item.end_ns = None;
            item.closed_by = None;
            terminal
                .draw(|frame| {
                    node(
                        frame,
                        focus,
                        machine,
                        frame.area(),
                        &[&item],
                        false,
                        3_306_000_000,
                    )
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains(action), "{text}");
            assert!(!text.contains("ago"), "{text}");
        }
    }
    #[test]
    fn every_fault_card_hides_history_and_shows_only_active_effects() {
        let app = app(&["n1"]);
        let focus = app.focus.as_ref().unwrap();
        let machine = &focus.world.as_ref().unwrap().machines[0];
        for action in [
            "drop_connection",
            "stop",
            "restart",
            "pause_process",
            "resume_process",
            "partition",
            "shape",
            "heal",
            "clock_skew",
            "clock_drift",
            "throttle",
            "pause",
            "resume",
            "crash",
            "reboot",
            "future_fault",
        ] {
            for state in [
                State::Active,
                State::Closed,
                State::Instant,
                State::Failed,
                State::Upcoming,
            ] {
                let item = Item {
                    key: crate::activity::Key::Fault(1),
                    lane: Lane::Machine("n1".into()),
                    action: action.into(),
                    target: "n1".into(),
                    handle: Some("f1".into()),
                    start_ns: 1_000_000_000,
                    end_ns: (state == State::Closed).then_some(2_000_000_000),
                    closed_by: (state == State::Closed).then(|| "heal".into()),
                    state,
                };
                let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
                terminal
                    .draw(|frame| {
                        node(
                            frame,
                            focus,
                            machine,
                            frame.area(),
                            &[&item],
                            false,
                            3_000_000_000,
                        )
                    })
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert_eq!(
                    text.contains(action),
                    state == State::Active,
                    "{action} {state:?}: {text}"
                );
            }
        }
    }
    #[test]
    fn machine_tile_shows_concurrent_faults_and_removes_only_healed_ones() {
        let mut app = app(&["n1", "n2", "n3", "oracle"]);
        let mut w = world(&["n1", "n2", "n3", "oracle"]);
        w.simulated_time_ns = 3_000_000_000;
        for (id, action, handle) in [(1, "throttle", "t1"), (2, "clock_skew", "c1")] {
            w.faults.push(FaultState {
                input_key: None,
                id,
                action: action.into(),
                machine: Some("n1".into()),
                status: FaultStatus::Applied,
                applied_time_ns: Some(1_000_000_000),
                plan: Some(serde_json::json!({"action":action,"machine":"n1","handle":handle})),
                active: true,
            });
        }
        for healed in [false, true] {
            w.revision += 1;
            w.faults[0].active = !healed;
            app.apply(Update::World(w.clone()));
            let focus = app.focus.as_ref().unwrap();
            let activity = crate::activity::derive(Some(&w), &[], &[], w.simulated_time_ns);
            app.hitboxes.borrow_mut().clear();
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| {
                    map(
                        frame,
                        &app,
                        focus,
                        &activity,
                        frame.area(),
                        w.simulated_time_ns,
                    )
                })
                .unwrap();
            let rect = app
                .hitboxes
                .borrow()
                .iter()
                .find(|(_, target)| *target == HitTarget::Machine(0))
                .unwrap()
                .0;
            let buffer = terminal.backend().buffer();
            let mut tile = String::new();
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    tile.push_str(buffer[(x, y)].symbol());
                }
                tile.push('\n');
            }
            assert_eq!(tile.contains("throttle t1 · 2.0s"), !healed, "{tile}");
            assert!(tile.contains("clock_skew c1 · 2.0s"), "{tile}");
            assert_eq!(rect.height, if healed { 6 } else { 7 });
        }
    }
}
