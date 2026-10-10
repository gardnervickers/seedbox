use crate::map;
use crate::model::{parse_chunks, Chunk, Counts, LinkState, Policy, Run};
use crate::ui::{self, App};
use ratatui::layout::Rect;
use ratatui::{backend::TestBackend, Terminal};
use std::{collections::BTreeMap, fs, path::PathBuf};

/// The #60 link-policy host run L1: shapes, heals, loss and a partition over a,b,c.
fn link60() -> Run {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/link60");
    Run::load(&dir).unwrap()
}

fn policy(latency_ms: u64, loss_ppm: u64) -> Policy {
    Policy {
        latency_ns: latency_ms * 1_000_000,
        jitter_ns: 0,
        loss_ppm,
        bandwidth_bps: None,
    }
}

fn link(run: &Run, at_ns: u64, from: &str, to: &str) -> LinkState {
    let i = |n: &str| run.machines.iter().position(|m| m == n).unwrap();
    run.snapshot(at_ns).links[&(i(from), i(to))].clone()
}

fn shaped(latency_ms: u64, loss_ppm: u64, handle: &str) -> LinkState {
    LinkState::Shaped {
        policy: policy(latency_ms, loss_ppm),
        handle: handle.into(),
        network_faults: false,
    }
}

#[test]
fn baselines_apply_before_any_fault() {
    let run = link60();
    assert_eq!(run.machines, ["a", "b", "c"]);
    assert_eq!(link(&run, 0, "a", "b"), LinkState::Normal(policy(3, 0)));
    assert_eq!(link(&run, 0, "b", "a"), LinkState::Normal(policy(1, 0)));
    assert_eq!(
        link(&run, 2_499_999_999, "a", "c"),
        LinkState::Normal(policy(1, 0))
    );
}

#[test]
fn highest_live_shape_wins_and_heal_reveals_the_layer_below() {
    let run = link60();
    // ac_old (5ms, #1) then ac_new (8ms, #3) on a->c; same for b->c.
    assert_eq!(link(&run, 2_500_000_000, "a", "c"), shaped(5, 0, "ac_old"));
    assert_eq!(link(&run, 3_000_000_000, "a", "c"), shaped(8, 0, "ac_new"));
    // Healing the lower layer keeps the newer one; healing the newer reveals the older.
    assert_eq!(link(&run, 3_500_000_000, "a", "c"), shaped(8, 0, "ac_new"));
    assert_eq!(link(&run, 3_500_000_000, "b", "c"), shaped(5, 0, "bc_old"));
    assert_eq!(
        link(&run, 4_000_000_000, "b", "c"),
        LinkState::Normal(policy(1, 0))
    );
}

#[test]
fn partitions_cut_both_directions_and_dominate_shapes() {
    let run = link60();
    let cut = LinkState::Cut {
        handle: "cut".into(),
    };
    assert_eq!(link(&run, 4_300_000_000, "c", "a"), cut);
    assert_eq!(link(&run, 4_300_000_000, "a", "c"), cut);
    assert_eq!(
        link(&run, 4_300_000_000, "c", "b"),
        shaped(1, 1_000_000, "lossall")
    );
    // After the heal, the lossy shape under the cut shows again.
    let LinkState::Shaped { policy, handle, .. } = link(&run, 4_500_000_000, "c", "a") else {
        panic!("c->a should be shaped after the partition heals");
    };
    assert_eq!((policy.loss_ppm, handle.as_str()), (500_000, "lossmid"));
}

#[test]
fn interval_traffic_adds_up_to_the_final_counters() {
    let run = link60();
    let mut total: BTreeMap<_, Counts> = BTreeMap::new();
    let stops = run.stops();
    for &t in &stops[..stops.len() - 1] {
        let snap = run.snapshot(t);
        let interval = snap
            .interval
            .expect("every stop before the end has counters");
        assert_eq!(interval.start_ns, t);
        for (pair, c) in interval.traffic {
            let sum = total.entry(pair).or_default();
            sum.delivered += c.delivered;
            sum.partition_dropped += c.partition_dropped;
            sum.loss_dropped += c.loss_dropped;
        }
    }
    let last = run.final_counts.as_ref().unwrap();
    for (pair, c) in last {
        assert_eq!(total[pair].delivered, c.delivered, "{pair:?}");
        assert_eq!(total[pair].dropped(), c.dropped(), "{pair:?}");
    }
}

#[test]
fn total_loss_shows_as_drops_in_its_interval() {
    let run = link60();
    let snap = run.snapshot(4_100_000_000);
    let interval = snap.interval.unwrap();
    assert_eq!(
        (interval.start_ns, interval.end_ns),
        (4_000_000_000, 4_200_000_000)
    );
    let c_to_b = interval.traffic[&(2, 1)];
    // Frames already admitted at the boundary still count as delivered.
    assert!(c_to_b.loss_dropped > 100 * c_to_b.delivered, "{c_to_b:?}");
}

fn write_report(report: serde_json::Value) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "seedbox-scope-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("output/machines")).unwrap();
    fs::write(dir.join("output/vmm.json"), report.to_string()).unwrap();
    dir
}

#[test]
fn machine_lifecycle_follows_crash_reboot_pause_and_skew() {
    let row = |id: u64, at: u64, action: &str, extra: serde_json::Value| {
        let mut request =
            serde_json::json!({"id": id, "at_ns": at, "action": action, "machine": "b"});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::json!({"id": id, "action": action, "status": "applied", "applied_at_ns": at, "request": request})
    };
    let mut skew = row(
        4,
        40,
        "clock_skew",
        serde_json::json!({"handle": "s", "max_offset_ns": 5_000_000}),
    );
    skew["offset_ns"] = serde_json::json!(-1_500_000);
    let dir = write_report(serde_json::json!({
        "seed": 7, "outcome": "end_machine_exited",
        "machines": [{"name": "a", "time_ns": 100, "state": "Exited(0)"}, {"name": "b", "time_ns": 90, "state": "Running"}],
        "faults": [
            row(1, 10, "crash", serde_json::json!({})),
            row(2, 20, "reboot", serde_json::json!({})),
            row(3, 30, "pause", serde_json::json!({})),
            skew,
            row(5, 50, "resume", serde_json::json!({})),
            row(6, 60, "heal", serde_json::json!({"handle": "s"})),
        ],
    }));
    let run = Run::load(&dir).unwrap();
    let b = |t| run.snapshot(t).machines[1].clone();
    assert_eq!(b(5).life, "running");
    assert_eq!(b(10).life, "down");
    assert_eq!((b(20).life, b(20).incarnation), ("running", 1));
    assert_eq!(b(30).life, "paused");
    assert_eq!(b(45).effects, ["clock_skew b -1.5ms"]);
    assert_eq!((b(55).life, b(55).effects.len()), ("running", 1));
    assert!(b(65).effects.is_empty());
    assert_eq!(
        run.snapshot(100).machines[0].final_state.as_deref(),
        Some("Exited(0)")
    );
    fs::remove_dir_all(dir).unwrap();
}

fn chunk_index(chunks: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut bytes = b"SBLOG001".to_vec();
    for (order, &(time, offset, length)) in chunks.iter().enumerate() {
        for field in [time, 0, order as u64, offset, length] {
            bytes.extend(field.to_le_bytes());
        }
    }
    bytes
}

#[test]
fn console_tail_stops_at_the_cursor_time() {
    let dir = write_report(serde_json::json!({"machines": [{"name": "a", "time_ns": 300}]}));
    let machines = dir.join("output/machines");
    fs::write(machines.join("a.console"), "boot\nready\nlate\n").unwrap();
    fs::write(
        machines.join("a.console.chunks"),
        chunk_index(&[(100, 0, 5), (200, 5, 6), (300, 11, 5)]),
    )
    .unwrap();
    let run = Run::load(&dir).unwrap();
    let console = run.consoles[0].as_ref().unwrap();
    assert_eq!(console.tail(50, 1024).unwrap(), b"");
    assert_eq!(console.tail(250, 1024).unwrap(), b"boot\nready\n");
    assert_eq!(console.tail(250, 6).unwrap(), b"ready\n");
    assert_eq!(console.tail(300, 1024).unwrap(), b"boot\nready\nlate\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn chunk_index_rejects_other_formats() {
    assert_eq!(
        parse_chunks(&chunk_index(&[(7, 0, 3)])),
        Some(vec![Chunk {
            time_ns: 7,
            incarnation: 0,
            offset: 0,
            length: 3
        }])
    );
    assert_eq!(parse_chunks(b"SBLOG002"), None);
    assert_eq!(parse_chunks(&chunk_index(&[(7, 0, 3)])[..47]), None);
}

#[test]
fn the_table_view_shows_the_cut_and_the_fault_list() {
    let mut app = App::new(link60(), Some(4_300_000_000));
    app.grid = true;
    let mut terminal = Terminal::new(TestBackend::new(140, 44)).unwrap();
    terminal.draw(|f| ui::draw(f, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("t=4.300s / 6.017s"));
    assert!(text.contains("CUT [cut]"));
    assert!(text.contains("partition c | a [cut]"));
    assert!(text.contains("faults 12 of 16 applied"));
}

#[test]
fn stops_walk_fault_boundaries_in_order() {
    let mut app = App::new(link60(), None);
    let mut seen = vec![app.at_ns];
    loop {
        let before = app.at_ns;
        app.next_stop();
        if app.at_ns == before {
            break;
        }
        seen.push(app.at_ns);
    }
    assert_eq!(seen.first(), Some(&0));
    assert_eq!(seen.last(), Some(&app.run.end_ns));
    assert!(seen.contains(&4_300_000_000));
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
    app.previous_stop();
    assert_eq!(app.at_ns, seen[seen.len() - 2]);
}

#[test]
fn link_columns_fit_long_shaped_policies() {
    let mut app = App::new(link60(), Some(4_000_000_000));
    app.grid = true;
    let mut terminal = Terminal::new(TestBackend::new(140, 44)).unwrap();
    terminal.draw(|f| ui::draw(f, &app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("1ms±500us 50% [lossmid]"));
}

fn frame_text(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 44)).unwrap();
    terminal.draw(|f| ui::draw(f, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_map_draws_a_box_per_machine_and_labels_each_link() {
    let text = frame_text(&App::new(link60(), Some(4_300_000_000)));
    // The selected machine (a) gets a heavy border.
    for name in ["━ a ━", "─ b ─", "─ c ─"] {
        assert!(text.contains(name), "{name}\n{text}");
    }
    // Both directions of the partitioned pair are labelled cut; the total-loss shape shows drops.
    assert_eq!(text.matches("cut ").count(), 3, "{text}"); // two links plus the legend
    assert!(text.contains("1ms 100% 0/s ✗14.5k/s"), "{text}");
}

#[test]
fn map_boxes_stay_inside_the_panel_and_never_overlap() {
    // Each panel holds 16 boxes of 20x4 in some grid.
    for (w, h) in [(84, 20), (63, 32), (168, 16)] {
        let area = Rect::new(3, 5, w, h);
        for n in 1..=16 {
            let boxes = map::layout(area, n, 20);
            assert_eq!(boxes.len(), n);
            for (i, a) in boxes.iter().enumerate() {
                assert!(
                    area.contains(a.as_position())
                        && a.right() <= area.right()
                        && a.bottom() <= area.bottom()
                );
                for b in &boxes[i + 1..] {
                    assert!(!a.intersects(*b), "{w}x{h} n={n}: {a:?} overlaps {b:?}");
                }
            }
        }
    }
}

#[test]
fn larger_worlds_focus_the_map_on_the_selected_machine() {
    let names: Vec<String> = (0..6).map(|i| format!("m{i}")).collect();
    let counters = |frames: u64| {
        let mut all = serde_json::Map::new();
        for a in &names {
            for b in names.iter().filter(|b| *b != a) {
                all.insert(
                    format!("{a}->{b}"),
                    serde_json::json!({"delivered": frames, "partition_dropped": 0, "receiver_dropped": 0}),
                );
            }
        }
        serde_json::Value::Object(all)
    };
    let request = serde_json::json!({"id": 1, "at_ns": 10, "action": "partition", "handle": "p", "left": ["m0"], "right": ["m1"]});
    let dir = write_report(serde_json::json!({
        "seed": 1, "outcome": "end_machine_exited",
        "machines": names.iter().map(|n| serde_json::json!({"name": n, "time_ns": 1_000_000_000u64, "state": "Running"})).collect::<Vec<_>>(),
        "faults": [{"id": 1, "action": "partition", "status": "applied", "applied_at_ns": 10, "request": request, "network_counters": counters(0)}],
        "network_counters": counters(1000),
    }));
    let mut app = App::new(Run::load(&dir).unwrap(), Some(20));
    app.selected = 2;
    let text = frame_text(&app);
    // One label per pair with the selected machine, each showing both directions.
    assert_eq!(text.matches(" →1.0k/s ←1.0k/s").count(), 5, "{text}");
    // The cut between two other machines is drawn but not labelled: only the legend says cut.
    assert_eq!(text.matches("cut").count(), 1, "{text}");
    // Labels never cover a box.
    for name in &names {
        assert!(text.contains(&format!(" {name} ")), "{name}\n{text}");
    }
    app.selected = 0;
    assert_eq!(frame_text(&app).matches("cut").count(), 2);
    fs::remove_dir_all(dir).unwrap();
}
