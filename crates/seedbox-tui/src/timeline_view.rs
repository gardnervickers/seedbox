//! Recorded fault timeline and the client's saved inclusion edits.
use crate::timeline::TimelineDraft;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Cell, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};
use seedbox_api::{FaultStatus, TimelineEntry, WorldState};
use std::path::Path;

const ICE: Color = Color::Rgb(120, 200, 255);
const DIM: Color = Color::Rgb(95, 112, 132);
const TEXT: Color = Color::Rgb(200, 211, 223);
const BORDER: Color = Color::Rgb(34, 48, 65);

fn block(title: String) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(title)
}

fn observed_effect(entry: &TimelineEntry, world: Option<&WorldState>) -> (&'static str, Color) {
    let Some(world) = world else {
        return ("recorded", DIM);
    };
    if entry
        .id
        .strip_prefix("program:")
        .is_some_and(|key| world.override_rejections.contains_key(key))
    {
        return ("rejected", Color::Rgb(255, 90, 106));
    }
    let Some(fault) = world
        .faults
        .iter()
        .find(|fault| Some(fault.id) == entry.fault_id)
    else {
        return ("upcoming", DIM);
    };
    if fault.plan.as_ref() != Some(&entry.action) {
        return ("unavailable", DIM);
    }
    let amber = Color::Rgb(255, 181, 71);
    let green = Color::Rgb(89, 211, 140);
    match fault.status {
        FaultStatus::Unrequested | FaultStatus::Requested => ("pending", amber),
        FaultStatus::Applied if fault.active => ("applied active", amber),
        FaultStatus::Applied => ("applied", green),
        FaultStatus::Failed => ("failed", Color::Rgb(255, 90, 106)),
        FaultStatus::Unavailable => ("unavailable", DIM),
    }
}

fn recording_date(unix_ms: u64) -> String {
    let days = (unix_ms / 86_400_000) as i64 + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let seconds = unix_ms / 1000 % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    draft: &TimelineDraft,
    selected: usize,
    save_path: &Path,
    world: Option<&WorldState>,
    display: (
        bool,
        &std::cell::RefCell<Vec<(Rect, crate::model::HitTarget)>>,
    ),
) {
    let (details, hitboxes) = display;
    let [heading, body] = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).areas(area);
    let entries = draft.entries();
    let total = entries.len();
    let removed = draft.modified_count();
    let metadata = draft.metadata();
    let save_label = if draft.is_unsaved() {
        "WORKING COPY UNSAVED · w save definition"
    } else {
        "Working copy stored"
    };
    let save_error = draft
        .save_error()
        .map(|error| format!(" · {error}"))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            "{} · {}\nApplication: {} · Seed: {} · Recorded: {}\n{} included · {} removed · {} faults{}\n{}: {}{}",
            metadata.name, draft.source(),
            if metadata.application.is_empty() { "unknown" } else { &metadata.application },
            metadata.seed.map(|seed| seed.to_string()).unwrap_or_else(|| "unknown".into()),
            recording_date(metadata.recorded_at_unix_ms),
            total - removed, removed, total,
            world.map(|world| format!(" · live {}", crate::view::time(world.simulated_time_ns))).unwrap_or_default(),
            save_label, if details { save_path.display().to_string() } else { "locally".into() }, save_error,
        )).style(Style::new().fg(if draft.is_unsaved() { Color::Rgb(255, 181, 71) } else { TEXT }))
            .wrap(Wrap { trim: false }), heading,
    );
    if total == 0 {
        frame.render_widget(
            Paragraph::new("Faults you inject appear here; : opens the inject palette on the simulation screen")
                .block(block(" Fault timeline · no recorded faults ".into()))
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(DIM)),
            body,
        );
        return;
    }
    let (table_area, detail_area) = if body.width >= 100 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(body);
        (left, Some(right))
    } else {
        (body, None)
    };
    let visible = usize::from(table_area.height.saturating_sub(3));
    let start = selected.saturating_sub(visible.saturating_sub(1));
    for row in 0..visible {
        hitboxes.borrow_mut().push((
            Rect::new(
                table_area.x + 1,
                table_area.y + 2 + row as u16,
                table_area.width.saturating_sub(2),
                1,
            ),
            crate::model::HitTarget::Timeline(start + row),
        ));
    }
    let rows = entries.iter().map(|entry| {
        let action = entry.action.clone();
        let name = action
            .get("action")
            .and_then(|value| value.as_str())
            .unwrap_or("fault");
        let target = action
            .as_object()
            .map(|fields| {
                fields
                    .iter()
                    .filter(|(key, _)| key.as_str() != "action")
                    .map(|(key, value)| {
                        format!(
                            "{key}={}",
                            value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let included = draft.included_entry(entry);
        let (effect, effect_color) = observed_effect(entry, world);
        Row::new(vec![
            Cell::from(crate::view::time(entry.at_ns)),
            Cell::from(name.to_owned()),
            Cell::from(target),
            Cell::from(if included { "included" } else { "removed" }),
            Cell::from(effect).style(Style::new().fg(if included { effect_color } else { DIM })),
        ])
        .style(
            Style::new()
                .fg(if included { TEXT } else { DIM })
                .add_modifier(if included {
                    Modifier::empty()
                } else {
                    Modifier::DIM
                }),
        )
    });
    let mut state = TableState::default().with_selected(Some(selected.min(total - 1)));
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(12),
                Constraint::Min(4),
                Constraint::Length(8),
                Constraint::Length(14),
            ],
        )
        .header(
            Row::new([
                "Time",
                "Action",
                "Target / parameters",
                "Included",
                "Observed",
            ])
            .style(Style::new().fg(ICE)),
        )
        .block(block(if removed == 0 {
            " Recorded timeline ".into()
        } else {
            format!(" Edited timeline · {removed} removed ")
        }))
        .column_spacing(1)
        .row_highlight_style(
            Style::new()
                .bg(Color::Rgb(19, 36, 58))
                .add_modifier(Modifier::BOLD),
        ),
        table_area,
        &mut state,
    );
    if let Some(detail_area) = detail_area {
        let entry = &entries[selected.min(total - 1)];
        let action =
            serde_json::to_string_pretty(&entry.action).unwrap_or_else(|error| error.to_string());
        let position = entry
            .position
            .map(|position| {
                format!(
                    "Committed position\nBoundary: {}\nCompleted rounds: {}\nSimulated time: {} ns",
                    position.boundary_index, position.completed_rounds, position.simulated_time_ns,
                )
            })
            .unwrap_or_else(|| "Scheduled input · no committed position".into());
        let detail = if details {
            format!(
            "Source: {}\nFault ID: {}\nEdited timeline: {}\nObserved: {}\n\n{}\n\nEffect: {} ns\n\nAction\n{}",
            entry.id,
            entry
                .fault_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "unassigned".into()),
            if draft.included_entry(entry) {
                "included"
            } else {
                "removed"
            },
            observed_effect(entry, world).0,
            position,
            entry.at_ns,
            action,
        )
        } else {
            let parameters = entry
                .action
                .as_object()
                .map(|fields| {
                    fields
                        .iter()
                        .filter(|(key, _)| key.as_str() != "action")
                        .map(|(key, value)| {
                            format!(
                                "{key}: {}",
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| value.to_string())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            format!(
                "{} at {}\n{} · {}\n\n{}\n\nv shows technical details",
                entry.action["action"].as_str().unwrap_or("Fault"),
                crate::view::time(entry.at_ns),
                if draft.included_entry(entry) {
                    "included"
                } else {
                    "removed"
                },
                observed_effect(entry, world).0,
                parameters
            )
        };
        frame.render_widget(
            Paragraph::new(detail)
                .block(block(" Selected fault ".into()))
                .wrap(Wrap { trim: false })
                .style(Style::new().fg(TEXT)),
            detail_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use seedbox_api::{BoundaryPosition, TimelineEntry, TimelineOrigin};

    fn entry() -> TimelineEntry {
        TimelineEntry {
            id: "fault-1".into(),
            at_ns: 1_000_000,
            action: serde_json::json!({"action":"future_heal","handle":"split"}),
            origin: TimelineOrigin::Recorded,
            position: Some(BoundaryPosition {
                boundary_index: 4,
                completed_rounds: 3,
                simulated_time_ns: 1_000_000,
            }),
            fault_id: Some(1),
        }
    }

    #[test]
    fn renders_removed_rows_and_details_and_handles_small_terminals() {
        let mut draft = TimelineDraft::new("k7m2p9qx".parse().unwrap(), 123);
        draft.merge(&[entry()]).unwrap();
        draft.toggle(0);
        let mut terminal = Terminal::new(TestBackend::new(150, 32)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    frame.area(),
                    &draft,
                    0,
                    Path::new("/tmp/timeline.json"),
                    None,
                    (true, &Default::default()),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for expected in [
            "UNSAVED",
            "removed",
            "heal",
            "split",
            "Completed rounds: 3",
            "/tmp/timeline.json",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        let mut small = Terminal::new(TestBackend::new(40, 8)).unwrap();
        small
            .draw(|frame| {
                draw(
                    frame,
                    frame.area(),
                    &draft,
                    99,
                    Path::new("/tmp/timeline.json"),
                    None,
                    (true, &Default::default()),
                )
            })
            .unwrap();
    }

    #[test]
    fn observed_status_requires_actual_world_evidence() {
        let entry = entry();
        let mut world = WorldState {
            override_rejections: Default::default(),
            simulation: "k7m2p9qx".parse().unwrap(),
            revision: 1,
            simulated_time_ns: 9_000_000,
            machines: vec![],
            links: vec![],
            faults: vec![],
        };
        assert_eq!(observed_effect(&entry, None).0, "recorded");
        assert_eq!(observed_effect(&entry, Some(&world)).0, "upcoming");
        world.faults.push(seedbox_api::FaultState {
            input_key: None,
            id: 1,
            action: "heal".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(1_000_000),
            plan: Some(entry.action.clone()),
            active: false,
            counts: None,
        });
        assert_eq!(observed_effect(&entry, Some(&world)).0, "applied");
        world.faults[0].active = true;
        assert_eq!(observed_effect(&entry, Some(&world)).0, "applied active");
        world.faults[0].plan = None;
        assert_eq!(observed_effect(&entry, Some(&world)).0, "unavailable");
        let mut rejected = entry.clone();
        rejected.id = "program:missing-handle".into();
        rejected.fault_id = None;
        world
            .override_rejections
            .insert("missing-handle".into(), "handle is not active".into());
        assert_eq!(observed_effect(&rejected, Some(&world)).0, "rejected");
    }
    #[test]
    fn dates_render_in_utc() {
        assert_eq!(recording_date(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(recording_date(1_704_067_200_000), "2024-01-01 00:00:00 UTC");
    }

    #[test]
    fn save_failure_remains_visible_without_claiming_saved() {
        let directory_owner = tempfile::tempdir().unwrap();
        let directory = directory_owner.path().join("timeline");
        std::fs::write(&directory, b"block directory creation").unwrap();
        let mut draft = TimelineDraft::new("k7m2p9qx".parse().unwrap(), 123);
        assert!(draft.save(&directory).is_err());
        let mut terminal = Terminal::new(TestBackend::new(200, 24)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    frame.area(),
                    &draft,
                    0,
                    &directory,
                    None,
                    (true, &Default::default()),
                )
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("UNSAVED"));
        assert!(text.contains("w save definition"));
        assert!(!text.contains("Saved:"));
        std::fs::remove_file(directory).unwrap();
    }
}
