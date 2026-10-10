//! The time bar: one lane per machine or handle, fault spans and instants on
//! simulated time, upcoming inputs ahead of the playhead and checkpoints below.
use crate::activity::{Activity, Item, Key, Lane, State};
use crate::model::{Checkpoint, HitTarget};
use crate::view::{glyph, palette as p, short_time};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
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
    pub selected: Option<&'a Key>,
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
        let lane_rows = rows.saturating_sub(2) as usize;
        let hidden = self.lanes.len().saturating_sub(lane_rows);
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
        for (lane_index, lane) in self.lanes.iter().take(lane_rows).enumerate() {
            let y = inner.y + 1 + lane_index as u16;
            let mut cells: Vec<(char, Style)> = vec![(' ', Style::new()); track];
            if let Some(x) = playhead {
                cells[x] = ('│', Style::new().fg(p::ICE_DIM));
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
