//! Rendering. Layout is computed from whatever the world reports each frame.
use crate::model::{rate_label, App, Focus, Screen, Tone};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Line as CanvasLine, Painter, Shape},
        Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap,
    },
    Frame,
};
use seedbox_api::{
    EventKind, FaultAction, FaultStatus, FrameFate, LinkState, LogStream, MachineCondition,
    MachineState, Phase, PlaybackRate, TestVerdict,
};
use std::time::Instant;

const ICE: Color = Color::Rgb(120, 200, 255);
const ICE_DIM: Color = Color::Rgb(51, 85, 111);
const TEAL: Color = Color::Rgb(79, 224, 194);
const AMBER: Color = Color::Rgb(255, 181, 71);
const RED: Color = Color::Rgb(255, 90, 106);
const YELLOW: Color = Color::Rgb(233, 207, 91);
const GREEN: Color = Color::Rgb(89, 211, 140);
const ORANGE: Color = Color::Rgb(255, 143, 74);
const MAGENTA: Color = Color::Rgb(212, 140, 255);
const TEXT: Color = Color::Rgb(200, 211, 223);
const DIM: Color = Color::Rgb(95, 112, 132);
const FAINT: Color = Color::Rgb(42, 57, 75);
const BORDER: Color = Color::Rgb(34, 48, 65);
const BORDER_HI: Color = Color::Rgb(60, 83, 112);
const BAR: Color = Color::Rgb(15, 24, 36);
const SELECTED: Color = Color::Rgb(19, 36, 58);

/// Simulated nanoseconds as seconds with a thin group separator.
pub fn time(ns: u64) -> String {
    let s = format!("{:.6}", ns as f64 / 1e9);
    let (head, tail) = s.split_at(s.len() - 3);
    format!("{head} {tail} s")
}

fn count(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=9_999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn duration_ns(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{}ms", ns as f64 / 1e6)
    } else {
        format!("{}µs", ns as f64 / 1e3)
    }
}

fn block(title: &str, focus: bool) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focus { BORDER_HI } else { BORDER }))
        .title(Span::styled(
            format!(" {title} "),
            Style::new()
                .fg(if focus { Color::White } else { TEXT })
                .add_modifier(Modifier::BOLD),
        ))
}

fn source(text: &str) -> Line<'_> {
    Line::from(Span::styled(
        format!(" ⇄ {text} "),
        Style::new().fg(ICE_DIM),
    ))
    .right_aligned()
}

pub fn draw(frame: &mut Frame, app: &App) {
    let now = Instant::now();
    let area = frame.area();
    let [top, body, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(10),
        Constraint::Length(1),
    ])
    .areas(area);
    header(frame, app, top);
    match (app.screen, app.focus.as_ref()) {
        (Screen::Simulation, Some(focus)) => simulation(frame, app, focus, body, now),
        _ => catalogue(frame, app, body),
    }
    footer(frame, app, keys, now);
    if app.help {
        help(frame, area);
    }
}

fn header(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Block::new().style(Style::new().bg(BAR)), area);
    let tab = |label: &str, on: bool| {
        Span::styled(
            format!(" {label} "),
            if on {
                Style::new().fg(BAR).bg(ICE).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(DIM)
            },
        )
    };
    let mut spans = vec![
        Span::styled(" ◢◤ SEEDBOX ", Style::new().fg(ICE).bold()),
        Span::raw(" "),
        tab("1 catalogue", app.screen == Screen::Catalogue),
        Span::raw(" "),
    ];
    if app.focus.is_some() {
        spans.push(tab("2 simulation", app.screen == Screen::Simulation));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    if let Some(focus) = &app.focus {
        let mut right = vec![Span::styled(focus.id.to_string(), Style::new().fg(TEXT))];
        if let Some(resource) = &focus.resource {
            let config = &resource.spec.configuration;
            right.push(Span::styled(
                format!(
                    " · {} · seed {} · ",
                    config.name,
                    resource.spec.seed.unwrap_or(0)
                ),
                Style::new().fg(DIM),
            ));
            let (label, color) = phase(focus);
            right.push(Span::styled(label, Style::new().fg(color).bold()));
            let (verdict, color) = verdict(resource.status.verdict());
            right.push(Span::styled(
                format!(" · {verdict} "),
                Style::new().fg(color),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            area,
        );
    }
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
            return ("❚❚ paused".into(), YELLOW);
        }
        return ("● running".into(), GREEN);
    }
    match focus.resource.as_ref().map(|r| r.status.phase) {
        Some(Phase::Queued) => ("· queued".into(), DIM),
        Some(Phase::Preparing) => ("◌ preparing".into(), YELLOW),
        Some(Phase::Running) => ("● running".into(), GREEN),
        Some(Phase::Finalizing) => ("◌ finalizing".into(), YELLOW),
        Some(Phase::Completed) => ("■ completed".into(), DIM),
        None => ("connecting".into(), DIM),
    }
}

fn verdict(v: TestVerdict) -> (&'static str, Color) {
    match v {
        TestVerdict::Passed => ("passed", GREEN),
        TestVerdict::Failed => ("failed", RED),
        TestVerdict::Incomplete => ("incomplete", YELLOW),
        TestVerdict::Pending => ("pending", DIM),
    }
}

fn footer(frame: &mut Frame, app: &App, area: Rect, now: Instant) {
    if let Some(text) = &app.palette {
        let line = Line::from(vec![
            Span::styled(" fault ▸ ", Style::new().fg(BAR).bg(MAGENTA).bold()),
            Span::styled(format!(" {text}▌"), Style::new().fg(Color::White)),
            Span::styled(
                "   YAML or JSON, e.g. {action: crash, machine: db-1}",
                Style::new().fg(DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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
    let keys: &[(&str, &str)] = match app.screen {
        Screen::Simulation => &[
            ("space", "pause"),
            (".", "step"),
            ("+-", "rate"),
            ("tab", "machine"),
            ("p", "partition"),
            ("s", "shape"),
            ("x", "crash/reboot"),
            ("h", "heal"),
            (":", "fault"),
            ("c", "checkpoint"),
            ("b", "branch"),
            ("esc", "catalogue"),
            ("?", "help"),
        ],
        Screen::Catalogue => &[
            ("↑↓", "select"),
            ("⏎", "open"),
            ("r", "rerun"),
            ("b", "branch"),
            ("s", "stop"),
            ("d", "delete"),
            ("m", "baseline"),
            ("c", "compare"),
            ("q", "quit"),
        ],
    };
    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(BAR).bg(DIM).bold(),
        ));
        spans.push(Span::styled(format!(" {label}  "), Style::new().fg(DIM)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn help(frame: &mut Frame, area: Rect) {
    let w = 64.min(area.width);
    let h = 20.min(area.height);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let lines = [
        ("space", "pause or resume (POST controls setRate)"),
        (".", "step one scheduler round (POST controls step)"),
        ("+ / -", "change the playback rate"),
        ("tab ↑↓", "select a machine; its logs follow"),
        ("p", "partition the machine from all others, or heal it"),
        ("s", "shape its outbound links (4× latency, 3% loss)"),
        ("x", "crash the machine, or reboot it once crashed"),
        ("h", "heal every active partition and shape"),
        (":", "inject any fault action as YAML"),
        ("c", "capture a checkpoint"),
        ("b", "branch a new simulation from the newest checkpoint"),
        ("r", "rerun with the same retained inputs"),
        ("1 / 2", "catalogue / simulation"),
        ("q", "quit"),
    ];
    let text: Vec<Line> = lines
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!(" {k:<8}"), Style::new().fg(ICE).bold()),
                Span::styled(*v, Style::new().fg(TEXT)),
            ])
        })
        .collect();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text)
            .block(block("keys", true))
            .style(Style::new().bg(BAR)),
        rect,
    );
}

// ------------------------------------------------------------------ simulation

fn simulation(frame: &mut Frame, app: &App, focus: &Focus, area: Rect, now: Instant) {
    let [main, lower, timeline] = Layout::vertical([
        Constraint::Percentage(64),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(area);
    let [map_area, side] =
        Layout::horizontal([Constraint::Percentage(64), Constraint::Min(40)]).areas(main);
    let [logs_area, events_area] =
        Layout::horizontal([Constraint::Percentage(64), Constraint::Min(40)]).areas(lower);
    let machine_rows = focus.world.as_ref().map_or(1, |w| w.machines.len()) as u16;
    let [machines_area, faults_area] = Layout::vertical([
        Constraint::Length((machine_rows + 6).min(side.height.saturating_sub(5))),
        Constraint::Min(5),
    ])
    .areas(side);
    let sim_now = focus.now_ns(now);
    map(frame, focus, map_area, sim_now);
    machines(frame, focus, machines_area);
    faults(frame, focus, faults_area);
    match app.inspector(now) {
        Some(inspector) => {
            let color = if inspector.ok { GREEN } else { RED };
            let mut lines: Vec<Line> = inspector
                .request
                .lines()
                .map(|l| Line::styled(l.to_owned(), Style::new().fg(TEXT)))
                .collect();
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("← {}", inspector.result),
                Style::new().fg(color),
            ));
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
        None => logs(frame, focus, logs_area),
    }
    events(frame, focus, events_area);
    status_bar(frame, focus, timeline, sim_now);
}

/// Machine centres on an ellipse, in cell coordinates relative to `area`.
pub fn layout(names: &[String], area: Rect) -> Vec<(f64, f64)> {
    let n = names.len();
    let (cx, cy) = (area.width as f64 / 2.0, area.height as f64 / 2.0);
    if n == 1 {
        return vec![(cx, cy)];
    }
    let (rx, ry) = (
        (area.width as f64 / 2.0 - 10.0).max(4.0),
        (area.height as f64 / 2.0 - 3.0).max(2.0),
    );
    (0..n)
        .map(|i| {
            let angle = -std::f64::consts::FRAC_PI_2 + i as f64 * std::f64::consts::TAU / n as f64;
            (cx + rx * angle.cos(), cy + ry * angle.sin())
        })
        .collect()
}

struct Dashed(CanvasLine);

impl Shape for Dashed {
    fn draw(&self, painter: &mut Painter) {
        let l = &self.0;
        let (Some((x1, y1)), Some((x2, y2))) =
            (painter.get_point(l.x1, l.y1), painter.get_point(l.x2, l.y2))
        else {
            return;
        };
        let steps = (x1.abs_diff(x2)).max(y1.abs_diff(y2)).max(1);
        for i in 0..=steps {
            if i % 6 > 2 {
                continue;
            }
            let x = x1 as f64 + (x2 as f64 - x1 as f64) * i as f64 / steps as f64;
            let y = y1 as f64 + (y2 as f64 - y1 as f64) * i as f64 / steps as f64;
            painter.paint(x.round() as usize, y.round() as usize, l.color);
        }
    }
}

fn box_size(name: &str) -> (u16, u16) {
    ((name.chars().count() as u16 + 6).max(16), 4)
}

fn map(frame: &mut Frame, focus: &Focus, area: Rect, now: u64) {
    let in_flight = focus
        .frames
        .iter()
        .filter(|f| f.fate == FrameFate::Scheduled)
        .count();
    let title = focus.resource.as_ref().map_or("network".to_string(), |r| {
        format!("network · {}", r.spec.configuration.name)
    });
    let outer = block(&title, true)
        .title(
            Line::styled(
                format!(" {in_flight} frames in flight "),
                Style::new().fg(DIM),
            )
            .right_aligned(),
        )
        .title_bottom(source("GET world · frames"));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let Some(world) = &focus.world else {
        frame.render_widget(Paragraph::new("waiting for the world…").fg(DIM), inner);
        return;
    };
    let names: Vec<String> = world.machines.iter().map(|m| m.name.clone()).collect();
    // The bottom row holds the legend.
    let centres = layout(
        &names,
        Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        },
    );
    let index = |name: &str| names.iter().position(|n| n == name);
    let (w, h) = (inner.width as f64, inner.height as f64);
    let to_canvas = |(x, y): (f64, f64)| (x, h - y);
    let selected = focus.selected.as_deref();

    // Undirected pairs: the worse of the two directions decides the colour.
    let mut pairs: Vec<(usize, usize, Vec<&LinkState>)> = Vec::new();
    for link in &world.links {
        let (Some(a), Some(b)) = (index(&link.from), index(&link.to)) else {
            continue;
        };
        let key = (a.min(b), a.max(b));
        match pairs.iter_mut().find(|(x, y, _)| (*x, *y) == key) {
            Some(entry) => entry.2.push(link),
            None => pairs.push((key.0, key.1, vec![link])),
        }
    }
    let down = |i: usize| {
        matches!(
            world.machines[i].condition,
            MachineCondition::Crashed | MachineCondition::Exited
        )
    };
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, w])
        .y_bounds([0.0, h])
        .paint(|ctx| {
            let mut labels = Vec::new();
            for (a, b, links) in &pairs {
                let (x1, y1) = to_canvas(centres[*a]);
                let (x2, y2) = to_canvas(centres[*b]);
                let cut = links.iter().find_map(|l| l.partitioned_by.as_ref());
                let shaped = links.iter().find(|l| l.shaped_by.is_some());
                let involved = selected.is_some_and(|s| names[*a] == s || names[*b] == s);
                let color = if cut.is_some() {
                    RED
                } else if shaped.is_some() {
                    YELLOW
                } else if involved {
                    ICE_DIM
                } else {
                    FAINT
                };
                let line = CanvasLine::new(x1, y1, x2, y2, color);
                if cut.is_some() || down(*a) || down(*b) {
                    ctx.draw(&Dashed(line));
                } else {
                    ctx.draw(&line);
                }
                let (mx, my) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0 + 0.6);
                if let Some(handle) = cut {
                    labels.push((mx, my, format!("✂ {handle}"), RED));
                } else if let Some(link) = shaped {
                    labels.push((
                        mx,
                        my,
                        format!("{}→ {}", link.from, duration_ns(link.policy.latency_ns)),
                        YELLOW,
                    ));
                } else if involved {
                    if let Some(link) = links.first() {
                        labels.push((mx, my, duration_ns(link.policy.latency_ns), ICE_DIM));
                    }
                }
            }
            ctx.layer();
            for (x, y, text, color) in &labels {
                let half = text.chars().count() as f64 / 2.0;
                ctx.print(
                    x - half,
                    *y,
                    Span::styled(text.clone(), Style::new().fg(*color)),
                );
            }
            // Frames: a head and a short trail along their link.
            for frame in &focus.frames {
                let (Some(a), Some(b)) = (index(&frame.from), index(&frame.to)) else {
                    continue;
                };
                let span = frame
                    .due_ns
                    .unwrap_or(frame.sent_ns + 1_000_000)
                    .saturating_sub(frame.sent_ns)
                    .max(1);
                let mut progress = now.saturating_sub(frame.sent_ns) as f64 / span as f64;
                let (glyph, color) = match frame.fate {
                    FrameFate::Scheduled => ("●", kind_color(frame.bytes)),
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
                let (ax, ay) = to_canvas(centres[a]);
                let (bx, by) = to_canvas(centres[b]);
                // Run between box edges, not centres.
                let (lo, hi) = (0.18, 0.82);
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

    for (i, machine) in world.machines.iter().enumerate() {
        node(frame, focus, machine, centres[i], inner);
    }
    let legend = Line::from(vec![
        Span::styled("● small ", Style::new().fg(TEAL)),
        Span::styled("● medium ", Style::new().fg(ICE)),
        Span::styled("● large ", Style::new().fg(AMBER)),
        Span::styled("✕ partition ", Style::new().fg(RED)),
        Span::styled("× loss", Style::new().fg(YELLOW)),
    ]);
    if inner.height > 2 {
        frame.render_widget(
            Paragraph::new(legend),
            Rect::new(inner.x + 1, inner.y + inner.height - 1, inner.width - 2, 1),
        );
    }
}

/// Frames are coloured by size: control chatter, ordinary requests, bulk data.
fn kind_color(bytes: u32) -> Color {
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

fn node(frame: &mut Frame, focus: &Focus, m: &MachineState, centre: (f64, f64), inner: Rect) {
    let (bw, bh) = box_size(&m.name);
    let bw = bw.min(inner.width);
    let x = (inner.x as f64 + centre.0 - bw as f64 / 2.0)
        .round()
        .max(inner.x as f64) as u16;
    let y = (inner.y as f64 + centre.1 - 1.5)
        .round()
        .max(inner.y as f64) as u16;
    let x = x.min(inner.x + inner.width - bw);
    let y = y.min(inner.y + inner.height.saturating_sub(bh));
    let rect = Rect::new(x, y, bw, bh.min(inner.height));
    let selected = focus.selected.as_deref() == Some(m.name.as_str());
    let isolated = focus.world.as_ref().is_some_and(|w| {
        let mine: Vec<_> = w
            .links
            .iter()
            .filter(|l| l.from == m.name || l.to == m.name)
            .collect();
        !mine.is_empty() && mine.iter().all(|l| l.partitioned_by.is_some())
    });
    let (state, color) = condition(m);
    let border = if isolated || m.condition == MachineCondition::Crashed {
        RED
    } else if selected {
        ICE
    } else {
        BORDER_HI
    };
    let (tx, rx) = focus.world.as_ref().map_or((0, 0), |w| {
        w.links.iter().fold((0, 0), |(tx, rx), l| {
            (
                tx + if l.from == m.name {
                    l.counters.scheduled
                } else {
                    0
                },
                rx + if l.to == m.name {
                    l.counters.scheduled
                } else {
                    0
                },
            )
        })
    });
    let mut title = vec![Span::styled(
        format!(" {} ", m.name),
        Style::new()
            .fg(if selected { Color::White } else { TEXT })
            .bold(),
    )];
    if m.oracle {
        title.push(Span::styled("◆ ", Style::new().fg(MAGENTA)));
    }
    if isolated {
        title.push(Span::styled("✂ ", Style::new().fg(RED)));
    }
    let b = Block::bordered()
        .border_type(if selected {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(Style::new().fg(border))
        .title(Line::from(title))
        .style(Style::new().bg(if selected {
            SELECTED
        } else {
            Color::Rgb(11, 17, 25)
        }));
    let mut first = vec![
        Span::styled("● ", Style::new().fg(color).bold()),
        Span::styled(state, Style::new().fg(color)),
    ];
    if m.incarnation > 0 {
        first.push(Span::styled(
            format!(" i{}", m.incarnation),
            Style::new().fg(YELLOW),
        ));
    }
    let lines = vec![
        Line::from(first),
        Line::styled(
            format!("↑{} ↓{}", count(tx), count(rx)),
            Style::new().fg(DIM),
        ),
    ];
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(b), rect);
}

fn condition(m: &MachineState) -> (String, Color) {
    let mut text = match m.condition {
        MachineCondition::Starting => ("starting".to_string(), YELLOW),
        MachineCondition::Ready => ("ready".to_string(), GREEN),
        MachineCondition::Paused => ("paused".to_string(), YELLOW),
        MachineCondition::Crashed => ("crashed".to_string(), RED),
        MachineCondition::Exited => (
            format!("exited {}", m.exit_code.unwrap_or(0)),
            if m.exit_code == Some(0) { DIM } else { RED },
        ),
    };
    if let Some(permille) = m.throttle_permille {
        text.0
            .push_str(&format!(" ×{:.1}", permille as f64 / 1000.0));
    }
    text
}

fn machines(frame: &mut Frame, focus: &Focus, area: Rect) {
    let b = block("machines", false).title_bottom(source("GET world"));
    let Some(world) = &focus.world else {
        frame.render_widget(b, area);
        return;
    };
    let header =
        Row::new(["", "MACHINE", "STATE", "INC", "TX", "RX", "DROP"]).style(Style::new().fg(DIM));
    let rows: Vec<Row> = world
        .machines
        .iter()
        .map(|m| {
            let selected = focus.selected.as_deref() == Some(m.name.as_str());
            let (state, color) = condition(m);
            let (mut tx, mut rx, mut dropped) = (0, 0, 0);
            for l in &world.links {
                let c = &l.counters;
                if l.from == m.name {
                    tx += c.scheduled;
                    dropped += c.partition_dropped + c.loss_dropped + c.queue_dropped;
                }
                if l.to == m.name {
                    rx += c.scheduled;
                    dropped += c.receiver_dropped;
                }
            }
            Row::new(vec![
                Cell::from(if selected { "▶" } else { " " }).fg(ICE),
                Cell::from(m.name.clone()).fg(if selected { Color::White } else { TEXT }),
                Cell::from(state).fg(color),
                Cell::from(m.incarnation.to_string()).fg(if m.incarnation > 0 {
                    YELLOW
                } else {
                    TEXT
                }),
                Cell::from(count(tx)).fg(DIM),
                Cell::from(count(rx)).fg(DIM),
                Cell::from(count(dropped)).fg(if dropped > 0 { RED } else { DIM }),
            ])
            .style(if selected {
                Style::new().bg(SELECTED)
            } else {
                Style::new()
            })
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(11),
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(5),
        ],
    )
    .header(header)
    .block(b);
    frame.render_widget(table, area);
}

fn fault_target(plan: &FaultAction) -> String {
    let list = |v: &[String]| {
        if v.len() > 2 {
            format!("{}+{}", v[0], v.len() - 1)
        } else {
            v.join(",")
        }
    };
    match plan {
        FaultAction::Partition { left, right, .. } => format!("{} | {}", list(left), list(right)),
        FaultAction::Shape {
            from,
            to,
            latency_ns,
            ..
        } => format!("{}→{} {}", list(from), list(to), duration_ns(*latency_ns)),
        FaultAction::Heal { handle } => handle.clone(),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v.get("machine").and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or_default(),
    }
}

fn faults(frame: &mut Frame, focus: &Focus, area: Rect) {
    let active = focus
        .world
        .as_ref()
        .map_or(0, |w| w.faults.iter().filter(|f| f.active).count());
    let b = block("faults", false)
        .title(
            Line::styled(
                format!(" {active} active "),
                Style::new().fg(if active > 0 { YELLOW } else { DIM }),
            )
            .right_aligned(),
        )
        .title_bottom(source("GET world · POST controls"));
    let Some(world) = &focus.world else {
        frame.render_widget(b, area);
        return;
    };
    let visible = area.height.saturating_sub(3) as usize;
    let rows: Vec<Row> = world
        .faults
        .iter()
        .rev()
        .take(visible)
        .rev()
        .map(|f| {
            let color = match f.action.as_str() {
                "heal" | "reboot" | "resume" | "resume_process" => GREEN,
                "shape" | "throttle" | "clock_skew" | "clock_drift" => YELLOW,
                _ => RED,
            };
            let status = match f.status {
                FaultStatus::Unrequested => ("planned", DIM),
                FaultStatus::Requested => ("requested", YELLOW),
                FaultStatus::Applied => ("applied", GREEN),
                FaultStatus::Failed => ("failed", RED),
                FaultStatus::Unavailable => ("unavailable", ORANGE),
            };
            let target = f
                .plan
                .as_ref()
                .map(fault_target)
                .or_else(|| f.machine.clone())
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(if f.active { "▌" } else { " " }).fg(color),
                Cell::from(format!("#{}", f.id)).fg(DIM),
                Cell::from(f.action.clone()).fg(color),
                Cell::from(target).fg(TEXT),
                Cell::from(status.0).fg(status.1),
                Cell::from(f.applied_time_ns.map(time).unwrap_or_default()).fg(DIM),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(10),
            Constraint::Min(12),
            Constraint::Length(10),
            Constraint::Length(13),
        ],
    )
    .header(
        Row::new(["", "ID", "ACTION", "TARGET", "STATUS", "APPLIED"]).style(Style::new().fg(DIM)),
    )
    .block(b);
    frame.render_widget(table, area);
}

fn logs(frame: &mut Frame, focus: &Focus, area: Rect) {
    let machine = focus.selected.clone().unwrap_or_default();
    let incarnation = focus
        .world
        .as_ref()
        .and_then(|w| w.machines.iter().find(|m| m.name == machine))
        .map_or(0, |m| m.incarnation);
    let title = format!("logs · {machine} · incarnation {incarnation}");
    let b = block(&title, false).title_bottom(source("GET logs (SSE)"));
    let height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = focus
        .logs
        .get(&machine)
        .map(|log| {
            log.lines
                .iter()
                .rev()
                .take(height)
                .rev()
                .map(|l| {
                    let err = l.stream == LogStream::Stderr;
                    Line::from(vec![
                        Span::styled(format!("{} ", time(l.time_ns)), Style::new().fg(DIM)),
                        Span::styled(
                            if err { "err " } else { "out " },
                            Style::new().fg(if err { ORANGE } else { FAINT }),
                        ),
                        Span::styled(
                            l.text.clone(),
                            Style::new().fg(if err { ORANGE } else { TEXT }),
                        ),
                    ])
                })
                .collect()
        })
        .unwrap_or_else(|| vec![Line::styled("no output yet", Style::new().fg(DIM))]);
    frame.render_widget(Paragraph::new(lines).block(b), area);
}

fn event_text(kind: &EventKind) -> (String, String, Color) {
    match kind {
        EventKind::FaultChanged {
            id, action, status, ..
        } => (
            "fault".into(),
            format!("#{id} {action} {status:?}").to_lowercase(),
            match status {
                FaultStatus::Applied => YELLOW,
                FaultStatus::Failed | FaultStatus::Unavailable => RED,
                _ => DIM,
            },
        ),
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
        EventKind::ControlChanged { control } => (
            "control".into(),
            format!(
                "{} {}",
                control.request.request_id,
                match &control.state {
                    seedbox_api::ControlState::Accepted => "accepted",
                    seedbox_api::ControlState::Acknowledged { error: None, .. } => "acknowledged",
                    seedbox_api::ControlState::Acknowledged { .. } => "rejected",
                    seedbox_api::ControlState::Unsupported => "unsupported",
                    seedbox_api::ControlState::Interrupted => "interrupted",
                }
            ),
            ICE,
        ),
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

fn events(frame: &mut Frame, focus: &Focus, area: Rect) {
    let mut b = block("events", false).title_bottom(source("GET events (SSE)"));
    if focus.lost > 0 {
        b = b.title(
            Line::styled(format!(" {} lost ", focus.lost), Style::new().fg(ORANGE)).right_aligned(),
        );
    }
    let height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = focus
        .events
        .iter()
        .rev()
        .take(height)
        .rev()
        .map(|e| {
            let (kind, text, color) = event_text(&e.event);
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

fn status_bar(frame: &mut Frame, focus: &Focus, area: Rect, now: u64) {
    frame.render_widget(Block::new().style(Style::new().bg(BAR)), area);
    let limit = focus
        .resource
        .as_ref()
        .map_or(0, |r| r.spec.configuration.limits.simulated_time_ns);
    let (state, color) = phase(focus);
    let mut spans = vec![
        Span::styled(
            format!(" {} ", state.split(' ').next().unwrap_or("")),
            Style::new().fg(color).bold(),
        ),
        Span::styled(
            format!("t {} ", time(now)),
            Style::new().fg(Color::White).bold(),
        ),
    ];
    let bar_width = (area.width as usize).saturating_sub(90).clamp(10, 60);
    if limit > 0 {
        let pos =
            ((now as f64 / limit as f64) * bar_width as f64).min(bar_width as f64 - 1.0) as usize;
        let mut cells: Vec<(char, Color)> = (0..bar_width)
            .map(|i| {
                if i < pos {
                    ('━', ICE_DIM)
                } else {
                    ('─', FAINT)
                }
            })
            .collect();
        if let Some(world) = &focus.world {
            for f in &world.faults {
                if let Some(at) = f.applied_time_ns {
                    let i = ((at as f64 / limit as f64) * bar_width as f64) as usize;
                    if let Some(cell) = cells.get_mut(i.min(bar_width - 1)) {
                        *cell = ('▲', if f.action == "heal" { GREEN } else { RED });
                    }
                }
            }
        }
        cells[pos] = ('█', Color::White);
        spans.push(Span::styled("▕", Style::new().fg(DIM)));
        for (c, color) in cells {
            spans.push(Span::styled(c.to_string(), Style::new().fg(color)));
        }
        spans.push(Span::styled(
            format!("▏{} ", time(limit)),
            Style::new().fg(DIM),
        ));
    }
    if let Some((p, _)) = &focus.playback {
        let position = &p.state.position;
        spans.push(Span::styled(
            format!(
                " boundary {} · rounds {} ",
                position.boundary_index, position.completed_rounds
            ),
            Style::new().fg(TEXT),
        ));
        spans.push(Span::styled(
            rate_label(p.state.rate),
            Style::new()
                .fg(if matches!(p.state.rate, PlaybackRate::Paused) {
                    YELLOW
                } else {
                    GREEN
                })
                .bold(),
        ));
        if let Some(achieved) = p.state.achieved_rate {
            spans.push(Span::styled(
                format!(
                    " ≈{:.4}×",
                    achieved.simulated_time_ns as f64 / achieved.wall_time_ns.get() as f64
                ),
                Style::new().fg(DIM),
            ));
        }
        if !p.state.application_ready {
            spans.push(Span::styled(" · apps starting", Style::new().fg(YELLOW)));
        }
    }
    spans.push(Span::styled("  ⇄ playback", Style::new().fg(ICE_DIM)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ------------------------------------------------------------------- catalogue

fn catalogue(frame: &mut Frame, app: &App, area: Rect) {
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(65), Constraint::Min(6)]).areas(area);
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Min(40)]).areas(top);
    let tree = app.tree();
    let header = Row::new([
        "", "ID", "WORLD", "SEED", "PHASE", "VERDICT", "SIM TIME", "ORIGIN",
    ])
    .style(Style::new().fg(DIM));
    let rows: Vec<Row> = tree
        .iter()
        .enumerate()
        .map(|(i, (depth, s))| {
            let selected = i == app.list_selected;
            let (phase, color) = match s.status.phase {
                Phase::Queued => ("· queued", DIM),
                Phase::Preparing => ("◌ preparing", YELLOW),
                Phase::Running => ("● running", GREEN),
                Phase::Finalizing => ("◌ finalizing", YELLOW),
                Phase::Completed => ("■ completed", DIM),
            };
            let (v, vcolor) = verdict(s.status.verdict());
            let prefix = if *depth > 0 {
                format!("{}└ ", "  ".repeat(depth - 1))
            } else {
                String::new()
            };
            let origin = match (&s.metadata.parent, &s.metadata.checkpoint) {
                (Some(p), Some(c)) => format!("branch of {p} @ {c}"),
                (Some(p), None) => format!("from {p}"),
                _ => String::new(),
            };
            let marked = app.baseline.as_ref() == Some(&s.metadata.id);
            Row::new(vec![
                Cell::from(if selected {
                    "▶"
                } else if marked {
                    "◆"
                } else {
                    " "
                })
                .fg(ICE),
                Cell::from(format!("{prefix}{}", s.metadata.id)).fg(if selected {
                    Color::White
                } else {
                    TEXT
                }),
                Cell::from(s.oracle.clone()).fg(DIM),
                Cell::from(String::new()),
                Cell::from(phase).fg(color),
                Cell::from(v).fg(vcolor),
                Cell::from(time(s.status.simulated_time_ns)).fg(TEXT),
                Cell::from(origin).fg(FAINT),
            ])
            .style(if selected {
                Style::new().bg(SELECTED)
            } else {
                Style::new()
            })
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(16),
            Constraint::Length(10),
            Constraint::Length(0),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(13),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(
        block("simulations", true)
            .title(Line::styled(format!(" {} ", tree.len()), Style::new().fg(DIM)).right_aligned())
            .title_bottom(source("GET /v1/simulations")),
    );
    frame.render_widget(table, list_area);
    detail(frame, app, detail_area);
    comparison(frame, app, bottom);
}

fn detail(frame: &mut Frame, app: &App, area: Rect) {
    let b = block("detail", false).title_bottom(source("GET /v1/simulations"));
    let tree = app.tree();
    let Some((_, s)) = tree.get(app.list_selected) else {
        frame.render_widget(
            Paragraph::new("no simulations yet; start one with `seedbox run --detach`")
                .fg(DIM)
                .block(b),
            area,
        );
        return;
    };
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<12}"), Style::new().fg(DIM)),
            Span::styled(v, Style::new().fg(TEXT)),
        ])
    };
    let mut lines = vec![
        kv("id", s.metadata.id.to_string()),
        kv("revision", s.metadata.revision.to_string()),
        kv("oracle", s.oracle.clone()),
        kv(
            "parent",
            s.metadata
                .parent
                .as_ref()
                .map_or("—".into(), |p| p.to_string()),
        ),
        kv(
            "checkpoint",
            s.metadata.checkpoint.clone().unwrap_or("—".into()),
        ),
        kv(
            "stage",
            s.status
                .stage
                .map_or("—".into(), |st| format!("{st:?}").to_lowercase()),
        ),
        kv(
            "termination",
            s.status
                .termination
                .as_ref()
                .map_or("—".into(), |t| format!("{t:?}").to_lowercase()),
        ),
        Line::raw(""),
    ];
    let longest = s
        .status
        .stage_timings
        .values()
        .map(|t| t.elapsed_wall_ns)
        .max()
        .unwrap_or(1)
        .max(1);
    for (stage, timing) in &s.status.stage_timings {
        let width = (timing.elapsed_wall_ns as f64 / longest as f64 * 18.0).round() as usize;
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12}", format!("{stage:?}").to_lowercase()),
                Style::new().fg(DIM),
            ),
            Span::styled("▇".repeat(width.max(1)), Style::new().fg(ICE)),
            Span::styled(
                format!(" {:.1}s", timing.elapsed_wall_ns as f64 / 1e9),
                Style::new().fg(DIM),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).block(b), area);
}

fn comparison(frame: &mut Frame, app: &App, area: Rect) {
    let b = block("compare", false).title_bottom(source("GET compare/{other}"));
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
    fn the_simulation_screen_shows_only_what_the_api_reported() {
        let mut app = app(&["api", "db", "oracle"]);
        let mut w = world(&["api", "db", "oracle"]);
        w.simulated_time_ns = 2_000_000;
        w.faults.push(FaultState {
            id: 3,
            action: "partition".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(1_500_000),
            plan: Some(FaultAction::Partition {
                handle: "split".into(),
                left: vec!["api".into()],
                right: vec!["db".into()],
            }),
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
        app.apply(Update::Log(LogDelivery::Entry {
            sequence: 1,
            entry: LogEntry::Bytes {
                machine: "api".into(),
                incarnation: 0,
                simulated_time_ns: 1_200_000,
                stream: LogStream::Stdout,
                bytes: b"listening on :8080\n".to_vec(),
            },
        }));
        let screen = render(&app);
        for expected in ["api", "db", "oracle", "split", "listening on :8080"] {
            assert!(screen.contains(expected), "missing {expected}:\n{screen}");
        }
        assert!(!screen.contains("cache"));

        // A machine the server adds later appears without any client change.
        app.apply(Update::World(world(&["api", "cache", "db", "oracle"])));
        let screen = render(&app);
        assert!(screen.contains("cache"), "{screen}");
    }
}
