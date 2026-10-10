//! Terminal rendering of a run at the cursor time.
use crate::model::{policy_text, rate_text, LinkState, Run, Snapshot};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Cell, List, ListItem, Paragraph, Row, Table, Wrap},
    Frame,
};

pub struct App {
    pub run: Run,
    pub at_ns: u64,
    pub selected: usize,
    /// Keep the cursor at the end and reload as the run directory changes.
    pub follow: bool,
    pub error: Option<String>,
}

impl App {
    pub fn new(run: Run, at_ns: Option<u64>) -> Self {
        let at_ns = at_ns.unwrap_or(0).min(run.end_ns);
        Self {
            run,
            at_ns,
            selected: 0,
            follow: false,
            error: None,
        }
    }

    pub fn next_stop(&mut self) {
        if let Some(&t) = self.run.stops().iter().find(|&&t| t > self.at_ns) {
            self.at_ns = t;
        }
    }

    pub fn previous_stop(&mut self) {
        if let Some(&t) = self.run.stops().iter().rev().find(|&&t| t < self.at_ns) {
            self.at_ns = t;
        }
    }

    /// Move by a hundredth of the run.
    pub fn nudge(&mut self, forward: bool) {
        let step = (self.run.end_ns / 100).max(1);
        self.at_ns = if forward {
            (self.at_ns + step).min(self.run.end_ns)
        } else {
            self.at_ns.saturating_sub(step)
        };
    }

    pub fn select(&mut self, delta: isize) {
        let n = self.run.machines.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(n.max(1)) as usize;
    }
}

fn seconds(ns: u64) -> String {
    format!("{:.3}s", ns as f64 / 1e9)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let snap = app.run.snapshot(app.at_ns);
    let [header, timeline, main, console, help] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Percentage(30),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, header, app);
    draw_timeline(frame, timeline, app);
    let [links, side] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(main);
    draw_links(frame, links, app, &snap);
    let machine_rows = app.run.machines.len() as u16 + 2;
    let [machines, faults] = Layout::vertical([
        Constraint::Length(machine_rows.min(side.height / 2)),
        Constraint::Min(3),
    ])
    .areas(side);
    draw_machines(frame, machines, app, &snap);
    draw_faults(frame, faults, app);
    draw_console(frame, console, app);
    let mut keys =
        "←/→ fault boundary  [/] step 1%  ↑/↓ machine  g/G start/end  f follow  r reload  q quit"
            .to_owned();
    if app.follow {
        keys = format!("FOLLOWING  {keys}");
    }
    if let Some(e) = &app.error {
        keys = format!("reload failed: {e}");
    }
    frame.render_widget(Paragraph::new(keys).dark_gray(), help);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let run = &app.run;
    // Time first: a long run path is truncated, never the cursor.
    let mut spans = vec![
        Span::from("seedbox-scope ").bold(),
        Span::from(format!(
            "t={} / {}",
            seconds(app.at_ns),
            seconds(run.end_ns)
        ))
        .bold()
        .cyan(),
    ];
    if let Some(seed) = run.seed {
        spans.push(Span::from(format!("  seed {seed}")));
    }
    spans.push(Span::from(format!(
        "  {}  ",
        run.outcome.as_deref().unwrap_or("no report")
    )));
    spans.push(Span::from(run.output.display().to_string()).dark_gray());
    let notes = Line::from(app.run.notes.join("; ")).dark_gray();
    frame.render_widget(Paragraph::new(vec![Line::from(spans), notes]), area);
}

fn draw_timeline(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" faults over simulated time ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    if width == 0 || app.run.end_ns == 0 {
        return;
    }
    let end = app.run.end_ns as u128;
    let column = |t: u64| ((t as u128 * (width as u128 - 1)) / end) as usize;
    let mut marks = vec![0u32; width];
    for f in &app.run.faults {
        if let Some(t) = f.applied_at_ns {
            marks[column(t).min(width - 1)] += 1;
        }
    }
    let cursor = column(app.at_ns).min(width - 1);
    let spans: Vec<Span> = marks
        .iter()
        .enumerate()
        .map(|(x, &m)| {
            let glyph = if x == cursor {
                "◆"
            } else if m > 0 {
                "┃"
            } else {
                "─"
            };
            let style = if x == cursor {
                Style::new().cyan().bold()
            } else if m > 0 {
                Style::new().yellow()
            } else {
                Style::new().dark_gray()
            };
            Span::styled(glyph, style)
        })
        .collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn draw_links(frame: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let run = &app.run;
    let title = match &snap.interval {
        Some(i) => format!(
            " links: policy and frames/s over {}..{} ",
            seconds(i.start_ns),
            seconds(i.end_ns)
        ),
        None => " links: policy (no traffic counters at this time) ".into(),
    };
    let block = Block::bordered().title(title);
    let n = run.machines.len();
    let mut grid: Vec<Vec<Text>> = Vec::new();
    for (from, name) in run.machines.iter().enumerate() {
        let mut cells = vec![Text::from(name.as_str()).bold()];
        for to in 0..n {
            if from == to {
                cells.push(Text::from("·").dark_gray());
                continue;
            }
            let (policy, style) = match &snap.links[&(from, to)] {
                LinkState::Normal(p) => (policy_text(p), Style::new()),
                LinkState::Shaped {
                    policy,
                    handle,
                    network_faults,
                } => (
                    format!(
                        "{}{} [{handle}]",
                        policy_text(policy),
                        if *network_faults { " +f" } else { "" }
                    ),
                    Style::new().yellow(),
                ),
                LinkState::Cut { handle } => (format!("CUT [{handle}]"), Style::new().red().bold()),
            };
            let traffic = snap.interval.as_ref().and_then(|i| {
                let c = i.traffic.get(&(from, to))?;
                let mut spans = vec![Span::from(rate_text(i.rate(c.delivered), "/s")).green()];
                if c.dropped() > 0 {
                    spans.push(
                        Span::from(format!(" ✗{}", rate_text(i.rate(c.dropped()), "/s"))).red(),
                    );
                }
                Some(Line::from(spans))
            });
            let mut text = Text::from(Line::styled(policy, style));
            text.push_line(traffic.unwrap_or_else(|| Line::from("")));
            cells.push(text);
        }
        grid.push(cells);
    }
    // Each column fits its widest cell, so shaped policies are never cut.
    let widths: Vec<Constraint> = (0..=n)
        .map(|c| {
            let header = if c == 0 { 9 } else { run.machines[c - 1].len() };
            let cells = grid.iter().map(|r| r[c].width()).max().unwrap_or(0);
            Constraint::Length((header.max(cells) + 2) as u16)
        })
        .collect();
    let mut header = vec![Cell::from("from \\ to").dark_gray()];
    header.extend(run.machines.iter().map(|m| Cell::from(m.as_str()).bold()));
    let rows = grid
        .into_iter()
        .map(|r| Row::new(r.into_iter().map(Cell::from)).height(2));
    let table = Table::new(rows, widths)
        .header(Row::new(header))
        .block(block);
    frame.render_widget(table, area);
}

fn draw_machines(frame: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let items = app
        .run
        .machines
        .iter()
        .zip(&snap.machines)
        .enumerate()
        .map(|(i, (name, s))| {
            let state = s.final_state.clone().unwrap_or_else(|| s.life.to_owned());
            let color = match s.life {
                "running" => Color::Green,
                "down" | "killed" => Color::Red,
                _ => Color::Yellow,
            };
            let mut spans = vec![
                Span::from(if i == app.selected { "▶ " } else { "  " }),
                Span::from(format!("{name:<12}")).bold(),
                Span::styled(state, Style::new().fg(color)),
            ];
            if s.incarnation > 0 {
                spans.push(Span::from(format!(" inc {}", s.incarnation)).dark_gray());
            }
            for e in &s.effects {
                spans.push(Span::from(format!("  {e}")).magenta());
            }
            ListItem::new(Line::from(spans))
        });
    frame.render_widget(
        List::new(items).block(Block::bordered().title(" machines ")),
        area,
    );
}

fn draw_faults(frame: &mut Frame, area: Rect, app: &App) {
    let run = &app.run;
    let current = run
        .faults
        .iter()
        .rposition(|f| f.applied_at_ns.is_some_and(|t| t <= app.at_ns));
    // Keep the most recent applied fault in view.
    let visible = area.height.saturating_sub(2) as usize;
    let skip = current.map_or(0, |c| {
        (c + 1).saturating_sub(visible.saturating_sub(2).max(1))
    });
    let items = run.faults.iter().enumerate().skip(skip).map(|(i, f)| {
        let time = f.time_ns().map(seconds).unwrap_or_else(|| "-".into());
        let mut style = match (f.applied_at_ns, f.status.as_str()) {
            (_, "failed" | "unavailable") => Style::new().red(),
            (Some(t), _) if t <= app.at_ns => Style::new(),
            _ => Style::new().dark_gray(),
        };
        if Some(i) == current {
            style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        let status = if f.status == "applied" {
            String::new()
        } else {
            format!(" ({})", f.status)
        };
        ListItem::new(Line::styled(
            format!("{time:>9} #{:<3} {}{status}", f.id, f.summary()),
            style,
        ))
    });
    let title = format!(
        " faults {} of {} applied ",
        run.faults
            .iter()
            .filter(|f| f.applied_at_ns.is_some_and(|t| t <= app.at_ns))
            .count(),
        run.faults.len()
    );
    frame.render_widget(List::new(items).block(Block::bordered().title(title)), area);
}

fn draw_console(frame: &mut Frame, area: Rect, app: &App) {
    let name = &app.run.machines[app.selected];
    let console = app.run.consoles[app.selected].as_ref();
    let (title, body) = match console {
        None => (
            format!(" {name} console "),
            "no console output retained".to_owned(),
        ),
        Some(c) => {
            let timed = if c.chunks.is_some() {
                format!("up to {}", seconds(app.at_ns))
            } else {
                "untimed, whole stream".into()
            };
            let body = match c.tail(app.at_ns, 64 * 1024) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).replace('\r', ""),
                Err(e) => format!("{}: {e}", c.path.display()),
            };
            (format!(" {name} console, {timed} "), body)
        }
    };
    let block = Block::bordered().title(title);
    let height = block.inner(area).height as usize;
    let lines: Vec<&str> = body.lines().collect();
    let tail = lines[lines.len().saturating_sub(height)..].join("\n");
    frame.render_widget(
        Paragraph::new(tail).block(block).wrap(Wrap { trim: false }),
        area,
    );
}
