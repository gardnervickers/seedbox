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

fn latency(ns: u64) -> String {
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
        form.draw(frame, area);
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
                    Panel::Logs => "logs",
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
            ("tab", "move focus: machines · faults · logs"),
            ("↑ ↓  j k", "move within the focused panel"),
            ("a", "show every machine's logs"),
            ("e", "switch logs and raw server events"),
            ("[ ]", "zoom the time bar out / in"),
            ("PgUp PgDn", "scroll logs · End follows again"),
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
    let lanes = activity.lanes(focus.world.as_ref());
    let present: Vec<String> = focus.machines();
    let wide = area.width >= 130 && area.height >= 34;
    let timebar_height = if area.height >= 30 {
        (lanes.len() as u16 + 4).clamp(5, 10)
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
        None => map(frame, app, focus, &activity, map_area, sim_now),
    }
    if wide {
        let machine_rows = (present.len() as u16 + 3).clamp(4, side.height / 2);
        let [faults_area, machines_area] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(machine_rows)]).areas(side);
        faults(frame, app, focus, &activity, faults_area, sim_now, None);
        machines(frame, app, focus, machines_area, None);
    } else if app.panel == Panel::Machines {
        machines(frame, app, focus, side, Some(Panel::Machines));
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
    app.hitboxes.borrow_mut().push((logs_area, HitTarget::Logs));
    let failure = app
        .inspector(now)
        .filter(|inspector| app.details || !inspector.ok);
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
        selected: focus.fault_selected.as_ref(),
        now: sim_now,
        zoom: app.zoom,
        focused: app.panel == Panel::Faults,
    }
    .draw(frame, time_area, &app.hitboxes);
    status_bar(frame, focus, &activity, status, sim_now, app.details);
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn spinner() -> char {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    SPINNER[(ms / 90) as usize % SPINNER.len()]
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
            lines.push(heading("booting machines".into()));
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
                    let (state, color) = condition(m);
                    lines.push(Line::from(vec![
                        Span::styled(pad(&m.name, width), Style::new().fg(machine_color(&m.name))),
                        Span::styled(state, Style::new().fg(color)),
                    ]));
                }
            }
            if let Some(elapsed) = stage_elapsed(Stage::Booting) {
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
    Dashed,
    Dotted,
}

/// A link drawn on the braille canvas. Lines that cross an earlier link leave
/// a small gap, so crossings read as bridges rather than knots.
struct Link {
    from: (f64, f64),
    to: (f64, f64),
    color: Color,
    pattern: Pattern,
    gaps: Vec<(f64, f64)>,
}

impl Shape for Link {
    fn draw(&self, painter: &mut Painter) {
        let (Some((x1, y1)), Some((x2, y2))) = (
            painter.get_point(self.from.0, self.from.1),
            painter.get_point(self.to.0, self.to.1),
        ) else {
            return;
        };
        let gaps: Vec<(usize, usize)> = self
            .gaps
            .iter()
            .filter_map(|(x, y)| painter.get_point(*x, *y))
            .collect();
        let steps = x1.abs_diff(x2).max(y1.abs_diff(y2)).max(1);
        for i in 0..=steps {
            let visible = match self.pattern {
                Pattern::Solid => true,
                Pattern::Dashed => i % 6 < 4,
                Pattern::Dotted => i % 4 == 0,
            };
            if !visible {
                continue;
            }
            let x =
                (x1 as f64 + (x2 as f64 - x1 as f64) * i as f64 / steps as f64).round() as usize;
            let y =
                (y1 as f64 + (y2 as f64 - y1 as f64) * i as f64 / steps as f64).round() as usize;
            if gaps
                .iter()
                .any(|(gx, gy)| gx.abs_diff(x) <= 3 && gy.abs_diff(y) <= 3)
            {
                continue;
            }
            painter.paint(x, y, self.color);
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
    ((0.05..=0.95).contains(&t) && (0.05..=0.95).contains(&u))
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
                .max(22),
            6,
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
    let mut drawn: Vec<Link> = Vec::new();
    let mut labels: Vec<((f64, f64), String, Color)> = Vec::new();
    for (a, b) in &pairs {
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
        let (color, pattern) = if cut.is_some() {
            (RED, Pattern::Dashed)
        } else if is_down {
            (FAINT, Pattern::Dotted)
        } else if shaped.is_some() {
            (YELLOW, Pattern::Solid)
        } else {
            (traffic_color(traffic, involved), Pattern::Solid)
        };
        let from = to_canvas(ca);
        let to = to_canvas(cb);
        let gaps = drawn
            .iter()
            .filter_map(|other| intersection((from, to), (other.from, other.to)))
            .collect();
        // Labels sit off-centre so they avoid the middle, where lines cross.
        let at = (
            from.0 + (to.0 - from.0) * 0.36,
            from.1 + (to.1 - from.1) * 0.36 + 0.6,
        );
        if let Some(handle) = &cut {
            labels.push((at, format!("✂ {handle}"), RED));
        } else if let Some(link) = shaped {
            labels.push((at, format!("≈ {}", latency(link.policy.latency_ns)), YELLOW));
        } else if involved && !is_down {
            labels.push((at, format!("{}/s", rate(traffic)), ICE));
        }
        drawn.push(Link {
            from,
            to,
            color,
            pattern,
            gaps,
        });
    }
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
                let (Some(a), Some(b)) = (centre(&frame.from), centre(&frame.to)) else {
                    continue;
                };
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
                let (ax, ay) = to_canvas(a);
                let (bx, by) = to_canvas(b);
                let (lo, hi) = (0.2, 0.8);
                let t = lo + (hi - lo) * progress;
                if frame.fate == FrameFate::Scheduled && progress > 0.06 {
                    let tt = lo + (hi - lo) * (progress - 0.06);
                    ctx.print(
                        ax + (bx - ax) * tt,
                        ay + (by - ay) * tt,
                        Span::styled("•", Style::new().fg(dim(color))),
                    );
                }
                ctx.print(
                    ax + (bx - ax) * t,
                    ay + (by - ay) * t,
                    Span::styled(glyph, Style::new().fg(color).bold()),
                );
            }
        });
    frame.render_widget(canvas, inner);

    for (index, m) in world.machines.iter().enumerate() {
        let Some(c) = centre(&m.name) else {
            continue;
        };
        let last = activity
            .rows()
            .filter(|item| {
                item.lane == Lane::Machine(m.name.clone()) && item.state != State::Upcoming
            })
            .max_by_key(|item| (item.open(), item.end_ns.unwrap_or(item.start_ns)));
        let target = targets.contains(&m.name);
        let rect = node_rect(c, field, box_width, box_height);
        app.hitboxes
            .borrow_mut()
            .push((rect, HitTarget::Machine(index)));
        if compact {
            badge(frame, focus, m, rect, target);
        } else {
            node(frame, focus, m, rect, last, target, now);
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

fn dim(color: Color) -> Color {
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
    let (state, color) = condition(m);
    let (border, selected) = node_border(focus, m, target);
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

fn node(
    frame: &mut Frame,
    focus: &Focus,
    m: &MachineState,
    rect: Rect,
    last: Option<&Item>,
    target: bool,
    now: u64,
) {
    let (border, selected) = node_border(focus, m, target);
    let (state, color) = condition(m);
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
        Line::styled(
            sparkline(focus, &m.name, inner_width),
            Style::new().fg(if down(m) {
                FAINT
            } else {
                dim(machine_color(&m.name))
            }),
        ),
    ];
    if let Some(item) = last {
        let ago = now.saturating_sub(item.end_ns.unwrap_or(item.start_ns));
        let color = if item.open() {
            span_color(&item.action, true)
        } else {
            FAINT_TEXT
        };
        let suffix = if item.open() {
            format!(" · {}", duration(now.saturating_sub(item.start_ns)))
        } else {
            format!(" · {} ago", duration(ago))
        };
        lines.push(Line::styled(
            format!(
                "{} {}{suffix}",
                glyph(&item.action, item.state),
                item.action
            ),
            Style::new().fg(color),
        ));
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(b), rect);
}

fn condition(m: &MachineState) -> (String, Color) {
    let mut text = match m.condition {
        MachineCondition::Starting => ("◌ starting".to_string(), YELLOW),
        MachineCondition::Ready => ("● ready".to_string(), GREEN),
        MachineCondition::Paused => ("❚❚ paused".to_string(), AMBER),
        MachineCondition::Crashed => ("✕ crashed".to_string(), RED),
        MachineCondition::Exited => (
            format!("✕ exited {}", m.exit_code.unwrap_or(0)),
            if m.exit_code == Some(0) { DIM } else { RED },
        ),
    };
    if let Some(permille) = m.throttle_permille {
        text.0
            .push_str(&format!(" ×{:.1}", permille as f64 / 1000.0));
    }
    text
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
    ])
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
    let show_drop = width >= name_width + 2 + 14 + 14 + 6;
    let show_spark = width >= name_width + 2 + 14 + 14 + 6 + 8;
    let mut header = vec![Span::styled(
        format!(
            "  {}{}{}{}",
            pad("NAME", name_width),
            pad("STATE", 14),
            pad_left("TX/S", 7),
            pad_left("RX/S", 7)
        ),
        Style::new().fg(FAINT_TEXT),
    )];
    if show_drop {
        header.push(Span::styled(
            pad_left("DROP", 6),
            Style::new().fg(FAINT_TEXT),
        ));
    }
    if show_spark {
        header.push(Span::styled("  ACTIVITY", Style::new().fg(FAINT_TEXT)));
    }
    let mut lines = vec![Line::from(header)];
    let spark_width = width.saturating_sub(name_width + 2 + 14 + 14 + 6 + 2);
    for (index, m) in world.machines.iter().enumerate() {
        let selected = focus.selected.as_deref() == Some(m.name.as_str());
        let (state, color) = condition(m);
        let dropped: u64 = world
            .links
            .iter()
            .map(|l| {
                let c = &l.counters;
                (if l.from == m.name {
                    c.partition_dropped + c.loss_dropped + c.queue_dropped
                } else {
                    0
                }) + if l.to == m.name {
                    c.receiver_dropped
                } else {
                    0
                }
            })
            .sum();
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
            Span::styled(pad(&state, 14), Style::new().fg(color)),
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
            spans.push(Span::styled(
                pad_left(&count(dropped), 6),
                Style::new().fg(if dropped > 0 { RED } else { DIM }),
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
    let rows = log_rows(focus, area.width.saturating_sub(2));
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

/// Wrapped terminal rows are also the unit of scrolling and viewport anchoring.
/// Application output is shown as written; fault markers come from the world.
pub(super) fn log_rows(focus: &Focus, width: u16) -> Vec<Line<'static>> {
    let selected = focus.selected.as_deref();
    let width = usize::from(width.max(1));
    let mut entries: Vec<(u64, u8, Line<'static>)> = Vec::new();
    for (name, log) in focus
        .logs
        .iter()
        .filter(|(name, _)| selected.is_none_or(|selected| selected == name.as_str()))
    {
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
                }
            }
            if !p.state.application_ready {
                left.push(Span::styled(" · apps starting", Style::new().fg(YELLOW)));
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

fn summary_phase(s: &SimulationSummary) -> (&'static str, Color) {
    match s.status.phase {
        Phase::Queued => ("◌ queued", AMBER),
        Phase::Preparing => ("◌ preparing", YELLOW),
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
    let b = block("simulations", true)
        .title(
            Line::styled(
                format!(" {} · {running} running ", tree.len()),
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
    let show_name = width >= 96;
    let id_width = 26;
    let mut header = format!("  {}", pad("ID", id_width));
    if show_name {
        header.push_str(&pad("NAME", 18));
    }
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
    let start = app.list_selected.saturating_sub(visible.saturating_sub(1));
    for (row, (depth, s)) in tree.iter().enumerate().skip(start).take(visible) {
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
        if show_name {
            let name = if s.metadata.name.is_empty() {
                "—"
            } else {
                &s.metadata.name
            };
            spans.push(Span::styled(
                pad(name, 18),
                Style::new().fg(if *depth == 0 { TEXT } else { DIM }),
            ));
        }
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
        let y = inner.y + 1 + (row - start) as u16;
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
    let lines: Vec<Line> = visible
        .into_iter()
        .rev()
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
                Span::styled(
                    e.simulated_time_ns.map(time).unwrap_or_else(|| "—".into()),
                    Style::new().fg(DIM),
                ),
                Span::styled(format!(" {kind:<8} "), Style::new().fg(ICE_DIM)),
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
}
