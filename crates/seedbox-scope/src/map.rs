//! The network map: one box per machine, laid out to fit the panel, with each
//! directed link drawn as an arrow between boxes, colored by its state.
use crate::model::{duration, rate_text, Counts, LinkState, MachineState, Run, Snapshot};
use ratatui::{
    layout::Rect,
    style::{Color, Style, Stylize},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Context, Line as Segment},
        Block, BorderType, Clear, Paragraph,
    },
    Frame,
};

const BOX_HEIGHT: u16 = 4;

/// Box rectangles (inside `area`) for `n` machines: a ring when the boxes fit
/// around it without touching, otherwise the grid that leaves the most room
/// between boxes for the panel's shape.
pub fn layout(area: Rect, n: usize, box_width: u16) -> Vec<Rect> {
    let (w, h) = (area.width as f64, area.height as f64);
    let (bw, bh) = (
        box_width.min(area.width) as f64,
        BOX_HEIGHT.min(area.height) as f64,
    );
    let place = |cx: f64, cy: f64| {
        let x = (cx - bw / 2.0).clamp(0.0, w - bw).round() as u16;
        let y = (cy - bh / 2.0).clamp(0.0, h - bh).round() as u16;
        Rect::new(area.x + x, area.y + y, bw as u16, bh as u16)
    };
    match n {
        0 => return Vec::new(),
        1 => return vec![place(w / 2.0, h / 2.0)],
        2 => return vec![place(bw / 2.0, h / 2.0), place(w - bw / 2.0, h / 2.0)],
        _ => {}
    }
    // Boxes that touch or overlap, allowing one free column between neighbours.
    let crowded = |boxes: &[Rect]| {
        boxes.iter().enumerate().any(|(i, a)| {
            boxes[i + 1..].iter().any(|b| {
                a.x < b.right() + 1 && b.x < a.right() + 1 && a.y < b.bottom() && b.y < a.bottom()
            })
        })
    };
    // A ring keeps every link a visible chord between two boxes.
    let (rx, ry) = ((w - bw) / 2.0, (h - bh) / 2.0);
    let ring: Vec<Rect> = (0..n)
        .map(|i| {
            let a = -std::f64::consts::FRAC_PI_2 + std::f64::consts::TAU * i as f64 / n as f64;
            place(w / 2.0 + rx * a.cos(), h / 2.0 + ry * a.sin())
        })
        .collect();
    if !crowded(&ring) {
        return ring;
    }
    // Rows are about twice as tall as columns are wide, so weigh vertical room double.
    let spare = |cols: usize| {
        let rows = n.div_ceil(cols);
        (w / cols as f64 - bw - 1.0).min(2.0 * (h / rows as f64 - bh))
    };
    let cols = (1..=n)
        .max_by(|&a, &b| spare(a).total_cmp(&spare(b)))
        .unwrap_or(1);
    let rows = n.div_ceil(cols);
    (0..n)
        .map(|i| {
            let (c, r) = (i % cols, i / cols);
            place(
                w * (c as f64 + 0.5) / cols as f64,
                h * (r as f64 + 0.5) / rows as f64,
            )
        })
        .collect()
}

fn center(r: Rect) -> (f64, f64) {
    (
        r.x as f64 + r.width as f64 / 2.0,
        r.y as f64 + r.height as f64 / 2.0,
    )
}

/// Where the ray from the center of `r` toward (dx, dy) leaves the box, plus a gap.
fn exit(r: Rect, dx: f64, dy: f64) -> (f64, f64) {
    let (cx, cy) = center(r);
    let (hw, hh) = (r.width as f64 / 2.0 + 1.0, r.height as f64 / 2.0 + 0.8);
    let t = (hw / dx.abs().max(1e-9)).min(hh / dy.abs().max(1e-9));
    (cx + dx * t, cy + dy * t)
}

fn life_color(s: &MachineState) -> Color {
    match s.life {
        _ if s
            .final_state
            .as_deref()
            .is_some_and(|f| f.starts_with("Exited")) =>
        {
            Color::DarkGray
        }
        "running" => Color::Green,
        "down" | "killed" => Color::Red,
        _ => Color::Yellow,
    }
}

fn link_style(state: &LinkState, traffic: Option<&Counts>) -> (Color, bool) {
    let flowing = traffic.is_some_and(|c| c.delivered > 0);
    let dropping = traffic.is_some_and(|c| c.dropped() > 0);
    match state {
        LinkState::Cut { .. } => (Color::Red, true),
        _ if dropping => (Color::Magenta, false),
        LinkState::Shaped { .. } => (Color::Yellow, false),
        _ if flowing => (Color::Green, false),
        _ => (Color::DarkGray, false),
    }
}

fn label(state: &LinkState, traffic: Option<&Counts>, rate: impl Fn(u64) -> f64) -> Line<'static> {
    let (policy, color) = policy_short(state);
    let mut spans = vec![Span::styled(policy, Style::new().fg(color))];
    if let Some(c) = traffic {
        spans.push(Span::from(format!(" {}", rate_text(rate(c.delivered), "/s"))).green());
        if c.dropped() > 0 {
            spans.push(Span::from(format!(" ✗{}", rate_text(rate(c.dropped()), "/s"))).red());
        }
    }
    Line::from(spans)
}

/// Draw order and focus-mode visibility: cut, dropping, shaped, then healthy.
fn rank(color: Color) -> u8 {
    match color {
        Color::Red => 3,
        Color::Magenta => 2,
        Color::Yellow => 1,
        _ => 0,
    }
}

fn policy_short(state: &LinkState) -> (String, Color) {
    match state {
        LinkState::Cut { .. } => ("cut".to_owned(), Color::Red),
        LinkState::Shaped { policy, .. } => {
            let mut s = duration(policy.latency_ns);
            if policy.loss_ppm > 0 {
                s += &format!(" {}%", policy.loss_ppm / 10_000);
            }
            (s, Color::Yellow)
        }
        LinkState::Normal(p) => (duration(p.latency_ns), Color::Gray),
    }
}

/// Both directions of a pair as seen from the selected machine:
/// policy, then frames/s sent (→) and received (←), then drops.
fn pair_label(
    out_state: &LinkState,
    in_state: &LinkState,
    out: Option<&Counts>,
    inn: Option<&Counts>,
    rate: impl Fn(u64) -> f64,
) -> Line<'static> {
    let (p1, c1) = policy_short(out_state);
    let (p2, c2) = policy_short(in_state);
    let mut spans = vec![Span::styled(p1.clone(), Style::new().fg(c1))];
    if p2 != p1 {
        spans.push("/".dark_gray());
        spans.push(Span::styled(p2, Style::new().fg(c2)));
    }
    if out.is_some() || inn.is_some() {
        let delivered = |c: Option<&Counts>| c.map_or(0, |c| c.delivered);
        // A dead or cut pair says so by its drops alone.
        if delivered(out) + delivered(inn) > 0 {
            spans.push(format!(" →{}", rate_text(rate(delivered(out)), "/s")).green());
            spans.push(format!(" ←{}", rate_text(rate(delivered(inn)), "/s")).green());
        }
        let dropped = out.map_or(0, |c| c.dropped()) + inn.map_or(0, |c| c.dropped());
        if dropped > 0 {
            spans.push(format!(" ✗{}", rate_text(rate(dropped), "/s")).red());
        }
    }
    Line::from(spans)
}

/// Where along the line from `near` to `far` a label of `width` cells fits
/// without covering a box or an earlier label; the far end is preferred so
/// labels spread out around the ring.
fn free_spot(
    inner: Rect,
    boxes: &[Rect],
    taken: &mut Vec<Rect>,
    near: (f64, f64),
    far: (f64, f64),
    width: usize,
) -> (f64, f64) {
    let width = (width as u16).min(inner.width);
    let at = |(t, rows, cols): (f64, f64, f64)| {
        let x = near.0 + (far.0 - near.0) * t - width as f64 / 2.0 + cols;
        let y = (near.1 + (far.1 - near.1) * t).round() + rows;
        let x = x
            .clamp(inner.x as f64, (inner.right() - width) as f64)
            .round();
        let y = y.clamp(inner.y as f64, (inner.bottom() - 1) as f64);
        Rect::new(x as u16, y as u16, width, 1)
    };
    let free = |r: &Rect| {
        let padded = Rect::new(r.x.saturating_sub(1), r.y, r.width + 2, 1);
        !boxes
            .iter()
            .chain(taken.iter())
            .any(|b| b.intersects(padded))
    };
    // On the line near the far end first, then progressively further off it,
    // which short links between neighbouring boxes need.
    let mut candidates = Vec::new();
    for t in [0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9] {
        for rows in -3i32..=3 {
            for cols in (-16i32..=16).step_by(4) {
                let cost =
                    (t - 0.7f64).abs() * 10.0 + rows.abs() as f64 * 3.0 + cols.abs() as f64 / 2.0;
                candidates.push((cost, (t, rows as f64, cols as f64)));
            }
        }
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    let spot = candidates
        .into_iter()
        .map(|(_, c)| at(c))
        .find(free)
        .unwrap_or_else(|| at((0.5, 0.0, 0.0)));
    taken.push(spot);
    (spot.x as f64, spot.y as f64)
}

pub fn draw(frame: &mut Frame, area: Rect, run: &Run, snap: &Snapshot, selected: usize) {
    let mut title = match &snap.interval {
        Some(i) => format!(
            " network  {:.3}s..{:.3}s ",
            i.start_ns as f64 / 1e9,
            i.end_ns as f64 / 1e9
        ),
        None => " network ".into(),
    };
    let focus = run.machines.len() > 4;
    if focus {
        title += &format!(" links of {} (↑/↓ to change) ", run.machines[selected]);
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.bold())
        .title_bottom(
            Line::from(vec![
                " ── ".green(),
                "traffic ".into(),
                "── ".yellow(),
                "shaped ".into(),
                "── ".magenta(),
                "dropping ".into(),
                "╌╌ ".red(),
                "cut ".into(),
            ])
            .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let n = run.machines.len();
    if n == 0 || inner.width < 8 || inner.height < BOX_HEIGHT {
        return;
    }
    let box_width = run
        .machines
        .iter()
        .map(|m| m.len())
        .max()
        .unwrap_or(0)
        .max(16) as u16
        + 4;
    let boxes = layout(inner, n, box_width);
    let (ox, oy, h) = (inner.x as f64, inner.y as f64, inner.height as f64);
    // Canvas y grows upward; boxes are in terminal rows. Bounds of size - 1 map
    // each whole coordinate exactly onto a cell, so labels land where placed.
    let (cw, ch) = (inner.width as f64 - 1.0, h - 1.0);
    let to_canvas = |(x, y): (f64, f64)| (x - ox, ch - (y - oy));
    let traffic = |p| snap.interval.as_ref().and_then(|i| i.traffic.get(&p));
    let rate = |frames: u64| snap.interval.as_ref().map_or(0.0, |i| i.rate(frames));

    let mut segments: Vec<(f64, f64, f64, f64, Color, bool)> = Vec::new();
    let mut labels: Vec<(f64, f64, Line<'static>)> = Vec::new();
    // Past a handful of machines, an arrow and label per direction is unreadable:
    // draw one line per pair, label only the selected machine's pairs (near the
    // other machine, so labels spread around the ring), and show other pairs only
    // when something is wrong with them.
    let mut context = Vec::new();
    let mut taken: Vec<Rect> = Vec::new();
    for (&(from, to), state) in &snap.links {
        if focus {
            if from > to {
                continue;
            }
            let back = &snap.links[&(to, from)];
            let (there, here) = (traffic((from, to)), traffic((to, from)));
            let (c1, d1) = link_style(state, there);
            let (c2, d2) = link_style(back, here);
            let (color, dashed) = if rank(c2) > rank(c1) {
                (c2, d2)
            } else {
                (c1, d1)
            };
            let (a, b) = (center(boxes[from]), center(boxes[to]));
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let (s, e) = (exit(boxes[from], dx, dy), exit(boxes[to], -dx, -dy));
            if from != selected && to != selected {
                if rank(color) > 0 {
                    context.push((s.0, s.1, e.0, e.1, color, dashed));
                }
                continue;
            }
            segments.push((s.0, s.1, e.0, e.1, color, dashed));
            // Orient the label from the selected machine's side.
            let (out, inn, ps, pi, far) = if from == selected {
                (there, here, state, back, e)
            } else {
                (here, there, back, state, s)
            };
            let near = if from == selected { s } else { e };
            let text = pair_label(ps, pi, out, inn, rate);
            let (lx, ly) = free_spot(inner, &boxes, &mut taken, near, far, text.width());
            labels.push((lx, ly, text));
            continue;
        }
        let (a, b) = (center(boxes[from]), center(boxes[to]));
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = (dx * dx + (2.0 * dy) * (2.0 * dy)).sqrt().max(1e-9);
        // Offset each direction to its own side; rows are about twice as tall as columns.
        let (px, py) = (-2.0 * dy / len * 2.0, dx / len);
        let start = exit(boxes[from], dx, dy);
        let end = exit(boxes[to], -dx, -dy);
        let (s, e) = ((start.0 + px, start.1 + py), (end.0 + px, end.1 + py));
        let c = traffic((from, to));
        let (color, dashed) = link_style(state, c);
        segments.push((s.0, s.1, e.0, e.1, color, dashed));
        // Arrowhead at the destination, as a glyph pointing along the link.
        let angle = (-(e.1 - s.1) * 2.0).atan2(e.0 - s.0);
        let octant = ((angle / std::f64::consts::FRAC_PI_4).round() as i64).rem_euclid(8) as usize;
        let glyph = ["→", "↗", "↑", "↖", "←", "↙", "↓", "↘"][octant];
        labels.push((e.0 - 0.5, e.1, Line::from(glyph.bold().fg(color))));
        let text = label(state, c, rate);
        let width = text.width() as f64;
        let (mx, my) = ((s.0 + e.0) / 2.0 + px, (s.1 + e.1) / 2.0 + py);
        labels.push((mx - width / 2.0, my, text));
    }

    context.extend(segments);
    let segments = context;
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, cw])
        .y_bounds([0.0, ch])
        .paint(|ctx: &mut Context| {
            for &(x1, y1, x2, y2, color, dashed) in &segments {
                let ((x1, y1), (x2, y2)) = (to_canvas((x1, y1)), to_canvas((x2, y2)));
                if dashed {
                    let steps =
                        (((x2 - x1).powi(2) + (2.0 * (y2 - y1)).powi(2)).sqrt() / 1.5) as usize;
                    for k in (0..steps).step_by(2) {
                        let (t0, t1) = (k as f64 / steps as f64, (k + 1) as f64 / steps as f64);
                        ctx.draw(&Segment::new(
                            x1 + (x2 - x1) * t0,
                            y1 + (y2 - y1) * t0,
                            x1 + (x2 - x1) * t1,
                            y1 + (y2 - y1) * t1,
                            color,
                        ));
                    }
                } else {
                    ctx.draw(&Segment::new(x1, y1, x2, y2, color));
                }
            }
            for (x, y, text) in &labels {
                let (x, y) = to_canvas((*x, *y));
                ctx.print(x.max(0.0), y, text.clone());
            }
        });
    frame.render_widget(canvas, inner);

    for (i, (r, s)) in boxes.iter().zip(&snap.machines).enumerate() {
        let color = life_color(s);
        let state = s.final_state.clone().unwrap_or_else(|| s.life.to_owned());
        let mut status = vec![Span::styled(state, Style::new().fg(color))];
        if s.incarnation > 0 {
            status.push(format!(" #{}", s.incarnation).dark_gray());
        }
        if let Some(e) = s.effects.first() {
            status.push(format!(" {}", e.split_whitespace().next().unwrap_or("")).magenta());
        }
        let (out, inn) = snap.interval.as_ref().map_or((0, 0), |iv| {
            iv.traffic.iter().fold((0, 0), |(o, n), (&(f, t), c)| {
                (
                    o + if f == i { c.delivered } else { 0 },
                    n + if t == i { c.delivered } else { 0 },
                )
            })
        });
        let io = Line::from(vec![
            "↑".dark_gray(),
            rate_text(rate(out), "/s").into(),
            " ↓".dark_gray(),
            rate_text(rate(inn), "/s").into(),
        ]);
        let border = if i == selected {
            BorderType::Thick
        } else {
            BorderType::Rounded
        };
        let block = Block::bordered()
            .border_type(border)
            .border_style(Style::new().fg(color))
            .title(
                Line::from(format!(" {} ", run.machines[i]))
                    .bold()
                    .centered(),
            );
        frame.render_widget(Clear, *r);
        frame.render_widget(
            Paragraph::new(vec![Line::from(status).centered(), io.centered()]).block(block),
            *r,
        );
    }
}
