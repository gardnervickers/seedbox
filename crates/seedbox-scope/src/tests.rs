use crate::model::{parse_chunks, Chunk, Counts, LinkState, Policy, Run};
use crate::ui::{self, App};
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
fn a_frame_shows_the_cut_and_the_fault_list() {
    let app = App::new(link60(), Some(4_300_000_000));
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
