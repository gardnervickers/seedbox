//! The time bar: one lane per machine or handle, fault spans and instants on
//! simulated time, upcoming inputs ahead of the playhead and checkpoints below.
use crate::activity::{Activity, Item, Key, Lane, State};
use crate::model::{Checkpoint, HitTarget, Sample};
use crate::view::{glyph, palette as p, short_time};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
    Frame,
};
use std::cell::RefCell;

pub struct TimeBar<'a> {
    pub activity: &'a Activity,
    pub lanes: &'a [Lane],
    pub present: &'a [String],
    pub checkpoints: &'a [Checkpoint],
    /// Traffic seen while watching, drawn beneath each machine's lane.
    pub traffic: &'a std::collections::BTreeMap<String, Vec<Sample>>,
    pub selected: Option<&'a Key>,
    /// The machine the logs are narrowed to, whose lane always shows.
    pub selected_machine: Option<&'a str>,
    pub now: u64,
    pub zoom: u32,
    pub focused: bool,
}

/// The visible window `[start, end)` in simulated nanoseconds.
pub fn window(activity: &Activity, now: u64, zoom: u32) -> (u64, u64) {
    let latest = activity
        .rows()
        .map(|item| item.end_ns.unwrap_or(item.start_ns))
        .chain(std::iter::once(now))
        .max()
        .unwrap_or(0);
    let full = (latest as f64 * 1.04).max(1e9) as u64;
    if zoom == 0 {
        return (0, full);
    }
    let span = (full >> zoom.min(40)).max(1_000_000);
    let end = (now + span / 4).min(full).max(span);
    (end.saturating_sub(span), end)
}

fn column(at: u64, (start, end): (u64, u64), width: usize) -> Option<usize> {
    if at < start || at > end || width == 0 {
        return None;
    }
    let fraction = (at - start) as f64 / (end - start).max(1) as f64;
    Some(((fraction * (width - 1) as f64).round() as usize).min(width - 1))
}

fn clamp_column(at: u64, (start, end): (u64, u64), width: usize) -> usize {
    column(at.clamp(start, end), (start, end), width).unwrap_or(0)
}

/// Tick spacing that gives roughly one label per 14 columns.
fn tick(span_ns: u64, width: usize) -> u64 {
    let target = span_ns as f64 / (width as f64 / 14.0).max(1.0);
    let steps = [1u64, 2, 5];
    let mut scale = 1_000_000u64;
    loop {
        for step in steps {
            let candidate = step * scale;
            if candidate as f64 >= target {
                return candidate;
            }
        }
        scale = scale.saturating_mul(10);
        if scale > 1_000_000_000_000_000 {
            return scale;
        }
    }
}

impl TimeBar<'_> {
    pub fn draw(&self, frame: &mut Frame, area: Rect, hitboxes: &RefCell<Vec<(Rect, HitTarget)>>) {
        let window = window(self.activity, self.now, self.zoom);
        let label_width = self
            .lanes
            .iter()
            .map(|lane| lane.label().chars().count() + 2)
            .max()
            .unwrap_or(6)
            .clamp(6, 14) as u16;
        let rows = area.height.saturating_sub(2);
        // The ruler always shows; the checkpoint row gives way when space is short.
        let lane_rows = rows.saturating_sub(if rows >= 4 { 2 } else { 1 }) as usize;
        // Short of rows, the last one names the lanes that did not fit.
        let shown_lanes = if self.lanes.len() > lane_rows {
            lane_rows.saturating_sub(1)
        } else {
            lane_rows
        };
        let hidden = self.lanes.len().saturating_sub(shown_lanes);
        let zoom_hint = if self.zoom == 0 {
            "[ ] zoom".to_string()
        } else {
            format!("zoom ×{} · [ ]", 1u64 << self.zoom.min(40))
        };
        let right = if hidden > 0 {
            format!(
                " {} ─ {} · +{hidden} lane{} · {zoom_hint} ",
                short_time(window.0),
                short_time(window.1),
                if hidden == 1 { "" } else { "s" }
            )
        } else {
            format!(
                " {} ─ {} · {zoom_hint} ",
                short_time(window.0),
                short_time(window.1)
            )
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if self.focused {
                p::BORDER_HI
            } else {
                p::BORDER
            }))
            .title(Span::styled(
                " time ",
                Style::new().fg(p::ICE).add_modifier(Modifier::BOLD),
            ))
            .title(Line::styled(right, Style::new().fg(p::DIM)).right_aligned());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width <= label_width + 4 || inner.height < 2 {
            return;
        }
        let track = (inner.width - label_width) as usize;
        let playhead = column(self.now, window, track);
        let mut lines: Vec<Line> = Vec::new();

        // Ruler.
        let mut ruler: Vec<(char, Style)> = vec![(' ', Style::new()); track];
        let step = tick(window.1 - window.0, track);
        let mut at = window.0.div_ceil(step) * step;
        while at <= window.1 {
            if let Some(x) = column(at, window, track) {
                let label = short_time(at);
                // Leave the playhead readable: skip a label that would cover it.
                if playhead.is_some_and(|p| p >= x && p < x + label.chars().count() + 1) {
                    at += step;
                    continue;
                }
                for (i, c) in label.chars().enumerate() {
                    if let Some(cell) = ruler.get_mut(x + i) {
                        *cell = (c, Style::new().fg(p::FAINT_TEXT));
                    }
                }
            }
            at += step;
        }
        if let Some(x) = playhead {
            ruler[x] = ('▼', Style::new().fg(p::ICE).add_modifier(Modifier::BOLD));
        }
        lines.push(row(" ".repeat(label_width as usize), Style::new(), ruler));

        let rows_of = |lane: &Lane| -> Vec<(usize, &Item)> {
            self.activity
                .rows()
                .enumerate()
                .filter(|(_, item)| &item.lane == lane)
                .collect()
        };
        // Traffic per machine, one column at a time: the busiest window in
        // each column, scaled to the busiest machine anywhere in view.
        let traffic: std::collections::BTreeMap<&str, Vec<Option<(f64, f64)>>> = self
            .traffic
            .iter()
            .map(|(name, samples)| (name.as_str(), columns(samples, window, track)))
            .collect();
        let peak = traffic
            .values()
            .flatten()
            .flatten()
            .map(|(sent, _)| *sent)
            .fold(1.0f64, f64::max);
        let (lanes, overflow) = self.visible_lanes(shown_lanes, &traffic);
        for (lane_index, lane) in lanes.into_iter().enumerate() {
            let y = inner.y + 1 + lane_index as u16;
            let mut cells: Vec<(char, Style)> = vec![(' ', Style::new()); track];
            if let Some(x) = playhead {
                cells[x] = ('│', Style::new().fg(p::ICE_DIM));
            }
            if let Lane::Machine(name) = lane {
                let shade = crate::view::dim(p::machine_color(name));
                for (cell, value) in cells
                    .iter_mut()
                    .zip(traffic.get(name.as_str()).into_iter().flatten())
                {
                    if let Some((sent, dropped)) = value {
                        if let Some(glyph) = level(*sent, *dropped, peak) {
                            let color = drop_color(shade, *sent, *dropped);
                            *cell = (glyph, Style::new().fg(color));
                        }
                    }
                }
            }
            let items = rows_of(lane);
            // Spans first, instants and upcoming markers on top.
            let mut ordered = items.clone();
            ordered.sort_by_key(|(_, item)| !item.is_span());
            for (row_index, item) in ordered {
                let selected = self.selected == Some(&item.key);
                let (from, to) = match item.state {
                    State::Active | State::Closed | State::Down => {
                        let end = item.end_ns.unwrap_or(self.now);
                        if end < window.0 || item.start_ns > window.1 {
                            continue;
                        }
                        (
                            clamp_column(item.start_ns, window, track),
                            clamp_column(end, window, track),
                        )
                    }
                    _ => match column(item.start_ns, window, track) {
                        Some(x) => (x, x),
                        None => continue,
                    },
                };
                let (fill, color) = match item.state {
                    State::Active => ('█', p::span_color(&item.action, true)),
                    State::Down => ('▓', p::RED_DEEP),
                    State::Closed => ('▬', p::span_color(&item.action, false)),
                    State::Upcoming => (glyph(&item.action, State::Upcoming), p::ICE),
                    State::Failed => ('✕', p::RED),
                    State::Instant => (
                        glyph(&item.action, State::Instant),
                        p::span_color(&item.action, true),
                    ),
                };
                // An upcoming heal draws a dashed bridge to its time from the
                // playhead, or from the upcoming fault it will close.
                if item.state == State::Upcoming && item.action == "heal" {
                    let opens = items
                        .iter()
                        .filter(|(_, other)| {
                            other.state == State::Upcoming
                                && other.action != "heal"
                                && other.handle == item.handle
                                && other.start_ns <= item.start_ns
                        })
                        .filter_map(|(_, other)| column(other.start_ns, window, track))
                        .max();
                    if let Some(start_x) = opens.or(playhead) {
                        for cell in cells.iter_mut().take(from).skip(start_x + 1) {
                            if cell.0 == ' ' || cell.0 == '│' {
                                *cell = ('┄', Style::new().fg(p::ICE_DIM));
                            }
                        }
                    }
                }
                let mut style = Style::new().fg(color);
                if selected {
                    style = style.bg(p::SELECTED).add_modifier(Modifier::BOLD);
                }
                for cell in cells.iter_mut().take(to + 1).skip(from) {
                    *cell = (fill, style);
                }
                // The leading edge of an open span breathes.
                if item.open() && crate::view::tick(500).is_multiple_of(2) {
                    cells[to] = ('▓', style);
                }
                hitboxes.borrow_mut().push((
                    Rect::new(
                        inner.x + label_width + from as u16,
                        y,
                        (to - from + 1) as u16,
                        1,
                    ),
                    HitTarget::Fault(row_index),
                ));
            }
            let gone = matches!(lane, Lane::Machine(name) if !self.present.contains(name));
            let label_color = match lane {
                Lane::Machine(_) if gone => p::FAINT_TEXT,
                Lane::Machine(_) => p::TEXT,
                Lane::Handle(_) => p::RED_SOFT,
                Lane::Other(_) => p::DIM,
            };
            let mut label: String = lane
                .label()
                .chars()
                .take(label_width as usize - 1)
                .collect();
            if gone && label.chars().count() + 2 < label_width as usize {
                label.push_str(" ⌀");
            }
            lines.push(row(
                format!("{label:<width$}", width = label_width as usize),
                Style::new().fg(label_color),
                cells,
            ));
        }

        if !overflow.is_empty() && lines.len() < rows as usize {
            let names: Vec<&str> = overflow.iter().map(|lane| lane.label()).collect();
            let text: Vec<(char, Style)> = names
                .join(", ")
                .chars()
                .chain(std::iter::repeat(' '))
                .take(track)
                .map(|c| (c, Style::new().fg(p::FAINT_TEXT)))
                .collect();
            lines.push(row(
                format!(
                    "{:<width$}",
                    format!("+{hidden} more"),
                    width = label_width as usize
                ),
                Style::new().fg(p::DIM),
                text,
            ));
        }

        // Checkpoints row, at the bottom when there is room.
        if lines.len() < rows as usize {
            let mut cells: Vec<(char, Style)> = vec![(' ', Style::new()); track];
            if let Some(x) = playhead {
                cells[x] = ('│', Style::new().fg(p::ICE_DIM));
            }
            for checkpoint in self.checkpoints {
                if let Some(x) = column(checkpoint.at_ns, window, track) {
                    cells[x] = ('◆', Style::new().fg(p::TEAL));
                }
            }
            lines.push(row(
                format!("{:<width$}", "ckpt", width = label_width as usize),
                Style::new().fg(p::DIM),
                cells,
            ));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

impl<'a> TimeBar<'a> {
    /// The lanes that fit in `rows`, in their usual order, and the rest.
    /// Short of rows, the selected machine comes first, then fault handles and
    /// lanes with something open or upcoming, then machines with traffic in view.
    fn visible_lanes(
        &self,
        rows: usize,
        traffic: &std::collections::BTreeMap<&str, Vec<Option<(f64, f64)>>>,
    ) -> (Vec<&'a Lane>, Vec<&'a Lane>) {
        if self.lanes.len() <= rows {
            return (self.lanes.iter().collect(), Vec::new());
        }
        let rank = |lane: &Lane| {
            let open = self
                .activity
                .rows()
                .any(|item| &item.lane == lane && (item.open() || item.state == State::Upcoming));
            match lane {
                Lane::Machine(name) if Some(name.as_str()) == self.selected_machine => 0,
                Lane::Handle(_) => 1,
                _ if open => 1,
                Lane::Machine(name)
                    if traffic
                        .get(name.as_str())
                        .is_some_and(|columns| columns.iter().any(Option::is_some)) =>
                {
                    2
                }
                _ => 3,
            }
        };
        let mut order: Vec<usize> = (0..self.lanes.len()).collect();
        order.sort_by_key(|&index| (rank(&self.lanes[index]), index));
        let mut keep = vec![false; self.lanes.len()];
        for &index in order.iter().take(rows) {
            keep[index] = true;
        }
        let (shown, hidden): (Vec<_>, Vec<_>) =
            self.lanes.iter().zip(keep).partition(|(_, keep)| *keep);
        (
            shown.into_iter().map(|(lane, _)| lane).collect(),
            hidden.into_iter().map(|(lane, _)| lane).collect(),
        )
    }
}

/// A machine's traffic colour, turning red as drops grow as a share of its
/// traffic: a few percent tints it, half or more is fully red. A busy machine
/// that drops a handful of frames to a cut peer keeps its own colour.
fn drop_color(shade: Color, sent: f64, dropped: f64) -> Color {
    let share = dropped / (sent + dropped).max(1.0);
    if share < 0.02 {
        return shade;
    }
    let t = (share / 0.5).min(1.0);
    match (shade, p::RED_SOFT) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix =
                |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ if t >= 0.5 => p::RED_SOFT,
        _ => shade,
    }
}

/// The busiest sample over each column, as (sent, dropped) frames per
/// second. A sample covers every column its window spans.
fn columns(samples: &[Sample], window: (u64, u64), width: usize) -> Vec<Option<(f64, f64)>> {
    let mut out: Vec<Option<(f64, f64)>> = vec![None; width];
    for sample in samples {
        if sample.at_ns < window.0 || sample.from_ns > window.1 {
            continue;
        }
        let from = clamp_column(sample.from_ns, window, width);
        let to = clamp_column(sample.at_ns, window, width);
        for cell in out.iter_mut().take(to + 1).skip(from) {
            let cell = cell.get_or_insert((0.0, 0.0));
            cell.0 = cell.0.max(sample.sent);
            cell.1 = cell.1.max(sample.dropped);
        }
    }
    out
}

/// A bar for a column's traffic relative to the busiest machine in view.
/// Quiet columns stay empty; any traffic or drops show at least a low mark.
fn level(sent: f64, dropped: f64, peak: f64) -> Option<char> {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if sent < 0.5 && dropped < 0.5 {
        return None;
    }
    Some(BARS[((sent / peak) * 7.0).round().clamp(0.0, 7.0) as usize])
}

fn row(label: String, label_style: Style, cells: Vec<(char, Style)>) -> Line<'static> {
    let mut spans = vec![Span::styled(label, label_style)];
    let mut run = String::new();
    let mut run_style = Style::new();
    for (c, style) in cells {
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_columns_keep_the_busiest_window_and_mark_drops() {
        let sample = |from_ns, at_ns, sent, dropped| Sample {
            from_ns,
            at_ns,
            sent,
            dropped,
        };
        let samples = [
            sample(0, 0, 10.0, 0.0),
            sample(0, 4, 30.0, 0.0),
            sample(60, 100, 0.0, 5.0),
        ];
        let columns = columns(&samples, (0, 100), 11);
        assert_eq!(columns[0], Some((30.0, 0.0)));
        assert_eq!(columns[1], None);
        // A window fills every column it spans.
        assert!(columns[6..].iter().all(|c| *c == Some((0.0, 5.0))));
        assert_eq!(level(0.0, 0.0, 100.0), None);
        assert_eq!(level(0.0, 5.0, 100.0), Some('▁'));
        assert_eq!(level(50.0, 0.0, 100.0), Some('▅'));
        assert_eq!(level(100.0, 0.0, 100.0), Some('█'));
    }

    #[test]
    fn lanes_redden_with_the_share_of_frames_dropped() {
        let shade = Color::Rgb(40, 80, 120);
        // A busy machine dropping a few frames to a cut peer keeps its colour.
        assert_eq!(drop_color(shade, 1000.0, 2.0), shade);
        assert_eq!(drop_color(shade, 0.0, 0.0), shade);
        // Half or more of its frames dropped is fully red.
        assert_eq!(drop_color(shade, 10.0, 10.0), p::RED_SOFT);
        assert_eq!(drop_color(shade, 0.0, 5.0), p::RED_SOFT);
        // In between, the colour moves toward red.
        let Color::Rgb(r, _, _) = drop_color(shade, 80.0, 20.0) else {
            panic!("expected rgb");
        };
        assert!(r > 40 && r < 224);
    }

    #[test]
    fn lanes_that_do_not_fit_are_named_and_the_important_ones_stay() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut lanes: Vec<Lane> = (1..=10).map(|i| Lane::Machine(format!("m{i}"))).collect();
        lanes.push(Lane::Handle("p1".into()));
        let present: Vec<String> = (1..=10).map(|i| format!("m{i}")).collect();
        let mut traffic = std::collections::BTreeMap::new();
        traffic.insert(
            "m7".to_string(),
            vec![Sample {
                from_ns: 0,
                at_ns: 500_000_000,
                sent: 40.0,
                dropped: 0.0,
            }],
        );
        let activity = Activity::default();
        let bar = TimeBar {
            activity: &activity,
            lanes: &lanes,
            present: &present,
            checkpoints: &[],
            traffic: &traffic,
            selected: None,
            selected_machine: Some("m9"),
            now: 1_000_000_000,
            zoom: 0,
            focused: false,
        };
        // Ten rows: borders, the ruler, five lanes, the overflow row and checkpoints.
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        let hitboxes = RefCell::new(Vec::new());
        terminal
            .draw(|frame| bar.draw(frame, frame.area(), &hitboxes))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let labels: Vec<&str> = rows[2..7]
            .iter()
            .map(|row| {
                row.trim_start_matches('│')
                    .split_whitespace()
                    .next()
                    .unwrap()
            })
            .collect();
        // The selected machine, the fault handle and the busy machine stay,
        // in their usual order, filled up with the first remaining lanes.
        assert_eq!(labels, ["m1", "m2", "m7", "m9", "p1"]);
        assert!(rows[7].contains("+6 more"), "{}", rows[7]);
        assert!(rows[7].contains("m3, m4, m5, m6, m8, m10"), "{}", rows[7]);
        assert!(rows[8].contains("ckpt"));
    }

    #[test]
    fn ticks_are_round_and_windows_follow_the_playhead() {
        assert_eq!(tick(180_000_000_000, 140), 20_000_000_000);
        assert_eq!(tick(1_000_000_000, 70), 200_000_000);
        let activity = Activity::default();
        let (start, end) = window(&activity, 100_000_000_000, 0);
        assert_eq!(start, 0);
        assert!(end >= 100_000_000_000);
        let (start, end) = window(&activity, 100_000_000_000, 3);
        assert!(start > 0 && start < 100_000_000_000 && end > 100_000_000_000);
        assert_eq!(column(50, (0, 100), 11), Some(5));
        assert_eq!(column(150, (0, 100), 11), None);
    }
}
