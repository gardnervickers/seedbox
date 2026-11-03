use super::*;
use crate::Completion;
use crate::{recorder::Recorder, History};

/// A history publishing each value, acknowledged at `Some(seq)` or left
/// unacknowledged (`info`) at `None`.
fn history(publishes: &[(&str, Option<u64>)]) -> Vec<Record<String, u64>> {
    let recorder = Recorder::with_clock(|| 0);
    for (value, ack) in publishes {
        let invoke = recorder.invoke(0, value.to_string());
        recorder.complete(
            invoke,
            match ack {
                Some(seq) => Completion::Ok(*seq),
                None => Completion::Info("timeout".into()),
            },
        );
    }
    recorder.records()
}

fn log(messages: &[(u64, &str)]) -> Result<Log, String> {
    Ok(Log::Messages(
        messages.iter().map(|(s, v)| (*s, v.to_string())).collect(),
    ))
}

fn reads(results: Vec<Result<Log, String>>) -> Vec<Read<Log>> {
    results
        .into_iter()
        .enumerate()
        .map(|(i, result)| Read {
            node: format!("n{}", i + 1),
            result,
        })
        .collect()
}

fn check(history: &[Record<String, u64>], results: Vec<Result<Log, String>>) -> Verdict {
    Queue::default().check(history, &reads(results))
}

#[test]
fn every_node_holding_every_ack_is_valid() {
    let h = history(&[("0-0", Some(1)), ("1-0", Some(2)), ("1-1", None)]);
    let all = || log(&[(1, "0-0"), (2, "1-0")]);
    let verdict = check(&h, vec![all(), all()]);
    assert_eq!(verdict.validity, Validity::Valid, "{verdict:?}");
    assert_eq!(
        verdict.summary,
        "2 acknowledged publishes present on every node"
    );
    // An unacknowledged publish may still appear.
    let verdict = check(&h, vec![log(&[(1, "0-0"), (2, "1-0"), (3, "1-1")]), all()]);
    assert_eq!(verdict.validity, Validity::Valid);
}

#[test]
fn lost_everywhere_is_lost_not_divergent() {
    let h = history(&[("0-0", Some(1)), ("0-1", Some(2)), ("0-2", Some(3))]);
    let held = || log(&[(1, "0-0"), (3, "0-2")]);
    let verdict = check(&h, vec![held(), held()]);
    assert_eq!(verdict.validity, Validity::Invalid);
    assert_eq!(verdict.details["lost"], 1);
    assert_eq!(verdict.details["divergent"], 0);
    assert_eq!(verdict.details["lost_classes"], json!({"lost-middle": 1}));
    assert_eq!(verdict.details["lost_seqs"], json!([2]));
    assert_eq!(
        verdict.summary,
        "3 of 3 acknowledged publishes checked: 1 lost"
    );
}

#[test]
fn divergence_conflicts_moves_and_unexpected_values() {
    let h = history(&[("0-0", Some(1)), ("0-1", Some(2))]);
    let verdict = check(
        &h,
        vec![
            log(&[(1, "0-0"), (2, "0-1")]),
            log(&[(1, "0-0"), (2, "9-9")]),
        ],
    );
    assert_eq!(verdict.validity, Validity::Invalid);
    for (key, count) in [
        ("lost", 0),
        ("divergent", 1),
        ("unexpected", 1),
        ("acked_seq_moved", 1),
    ] {
        assert_eq!(verdict.details[key], count, "{key}");
    }
    assert_eq!(verdict.details["seq_conflicts"], json!([2]));
}

#[test]
fn one_sequence_acknowledged_twice_and_a_missing_stream() {
    let h = history(&[("0-0", Some(1)), ("1-0", Some(1))]);
    let verdict = check(&h, vec![log(&[(1, "1-0")]), Ok(Log::StreamMissing)]);
    assert_eq!(verdict.validity, Validity::Invalid);
    assert_eq!(verdict.details["acked_seq_reused"], json!([1]));
    assert_eq!(verdict.details["stream_missing"], 1);
    assert_eq!(verdict.details["stream_missing_nodes"], json!(["n2"]));
}

#[test]
fn nothing_acknowledged_or_a_node_unread_cannot_decide() {
    let none = history(&[("0-0", None)]);
    let verdict = check(&none, vec![log(&[]), log(&[])]);
    assert_eq!(
        (verdict.validity, verdict.summary.as_str()),
        (Validity::Unknown, "no publish was acknowledged")
    );
    // The unread node might hold what no read node does.
    let h = history(&[("0-0", Some(1)), ("0-1", Some(2))]);
    let verdict = check(
        &h,
        vec![log(&[(1, "0-0")]), Err("connection refused".into())],
    );
    assert_eq!(verdict.validity, Validity::Unknown);
    assert_eq!(
        verdict.summary,
        "1 of 2 nodes were never read; 1 acknowledged publishes are on no node read"
    );
    assert_eq!(verdict.details["unread"], 1);
    assert_eq!(verdict.details["unread_nodes"], json!(["n2"]));
    // A definite finding still decides it.
    let verdict = check(&h, vec![log(&[(1, "0-0"), (2, "7-7")]), Err("down".into())]);
    assert_eq!(verdict.validity, Validity::Invalid);
}

#[test]
fn a_late_ack_after_info_counts_and_details_stay_small() {
    let recorder: Recorder<String, u64> = Recorder::with_clock(|| 0);
    let invoke = recorder.invoke(0, "0-0".into());
    recorder.complete(invoke, Completion::Info("timeout".into()));
    recorder.complete(invoke, Completion::Ok(1));
    let h = recorder.records();
    assert_eq!(check(&h, vec![log(&[])]).details["lost"], 1);
    // Thousands of findings keep the details within a property's bound.
    let publishes: Vec<_> = (0..2000).map(|i| format!("{}-{i}", i % 7)).collect();
    let acks: Vec<_> = publishes
        .iter()
        .enumerate()
        .map(|(i, v)| (v.as_str(), Some(i as u64 * 2)))
        .collect();
    let verdict = check(&history(&acks), vec![log(&[]), log(&[(1, "x")])]);
    assert_eq!(verdict.validity, Validity::Invalid);
    let size = serde_json::to_vec(&verdict.details).unwrap().len();
    assert!(size <= 1024, "{size}: {}", verdict.details);
    assert_eq!(
        ranges([5, 1, 2, 3, 9, 9]),
        [json!([1, 3]), json!(5), json!(9)]
    );
}

#[test]
fn duplicates_and_values_outside_p_c() {
    let h = history(&[("0-0", Some(1)), ("0-1", Some(2))]);
    let verdict = check(&h, vec![log(&[(1, "0-0"), (2, "0-1"), (3, "0-1")])]);
    assert_eq!(verdict.validity, Validity::Invalid);
    assert_eq!(verdict.details["duplicated"], 1);
    assert_eq!(verdict.details["duplicated_values"], json!(["0-1"]));
    assert!(
        verdict.summary.ends_with("1 duplicated"),
        "{}",
        verdict.summary
    );
    // Non-canonical and foreign values are not placed in a publisher's order.
    let h = history(&[
        ("1-1", Some(1)),
        ("01-2", Some(2)),
        ("x", Some(3)),
        ("1-3", Some(4)),
    ]);
    let verdict = check(&h, vec![log(&[(1, "1-1"), (4, "1-3")])]);
    assert_eq!(
        verdict.details["lost_classes"],
        json!({"lost-unordered": 2})
    );
    // Many unread nodes are counted, with a few named.
    let reads = (0..300).map(|_| Err("down".to_string())).collect();
    let verdict = check(&h, reads);
    assert_eq!(verdict.details["unread"], 300);
    assert_eq!(verdict.details["unread_nodes"].as_array().unwrap().len(), 5);
    assert!(serde_json::to_vec(&verdict.details).unwrap().len() <= 1024);
}
