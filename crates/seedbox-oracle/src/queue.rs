//! Jepsen's queue checks over a replicated log read back from every node.
//!
//! Each published value is unique (`P-C`: publisher and counter), each
//! acknowledgement carries the log sequence it was given, and each node's
//! final read is its own replica. The checker reports:
//!
//! * lost: acknowledged values no node holds, classified lost-prefix,
//!   lost-middle and lost-postfix per publisher against its read values
//!   (lost-unordered when the value is not a canonical `P-C`);
//! * divergent: acknowledged values some nodes hold and others lack;
//! * sequence conflicts: one sequence holding different values on two nodes;
//! * moved: an acknowledged value whose sequence holds another value on a node;
//! * reused: one sequence acknowledged for two values;
//! * duplicated: one value at two sequences of a node's log;
//! * a node whose stream is missing;
//! * unexpected: values that were never published.
//!
//! No acknowledged value, or a node never read, cannot decide: unknown,
//! unless another finding already decided it. A value no read node holds is
//! only lost once every node was read.
use crate::{recorder::completions, Checker, Event, Read, Record, Validity, Verdict};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// One node's final read of the log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Log {
    /// Sequence to value.
    Messages(BTreeMap<u64, String>),
    /// The node no longer knows the log.
    StreamMissing,
}

/// "Acknowledged publishes survive on every node", over histories whose
/// operations are the published values and whose `ok` values are the
/// acknowledged sequences.
pub struct Queue {
    pub name: String,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            name: "acknowledged publishes survive on every node".into(),
        }
    }
}

/// (publisher, counter) of a `P-C` value; other values sort last.
fn value_key(value: &str) -> (u64, u64) {
    publisher_counter(value).unwrap_or((u64::MAX, u64::MAX))
}

/// (publisher, counter) of a canonical `P-C` value: decimal, no sign, no
/// leading zero. Anything else is not a value this checker can place.
fn publisher_counter(value: &str) -> Option<(u64, u64)> {
    let canonical = |part: &str| {
        let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        (digits && (part == "0" || !part.starts_with('0')))
            .then(|| part.parse().ok())
            .flatten()
    };
    let (publisher, counter) = value.split_once('-')?;
    Some((canonical(publisher)?, canonical(counter)?))
}

/// Up to `n` of the first values, in publisher order.
fn first<'a>(values: impl IntoIterator<Item = &'a String>, n: usize) -> Vec<&'a String> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_by_key(|v| value_key(v));
    values.truncate(n);
    values
}

/// Sorted numbers as compact inclusive ranges: `[1, [3, 5], 9]`.
pub fn ranges(numbers: impl IntoIterator<Item = u64>) -> Vec<Value> {
    let mut sorted: Vec<u64> = numbers.into_iter().collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        while i + 1 < sorted.len() && sorted[i + 1] == sorted[i] + 1 {
            i += 1;
        }
        out.push(if sorted[i] == start {
            json!(start)
        } else {
            json!([start, sorted[i]])
        });
        i += 1;
    }
    out
}

fn classify(
    acked: &BTreeSet<&String>,
    union: &BTreeSet<&String>,
    lost: &BTreeSet<&String>,
) -> BTreeMap<&'static str, usize> {
    // Each publisher's acknowledged counters, with whether they were read.
    let mut by_publisher: BTreeMap<u64, Vec<(u64, &String)>> = BTreeMap::new();
    let mut classes = BTreeMap::new();
    for value in acked {
        match publisher_counter(value) {
            Some((publisher, counter)) => by_publisher
                .entry(publisher)
                .or_default()
                .push((counter, *value)),
            // A value outside P-C has no place in its publisher's order.
            None if lost.contains(value) => *classes.entry("lost-unordered").or_default() += 1,
            None => {}
        }
    }
    for counters in by_publisher.values() {
        let read: Vec<u64> = counters
            .iter()
            .filter(|(_, value)| union.contains(value))
            .map(|(counter, _)| *counter)
            .collect();
        let (min, max) = (read.iter().min(), read.iter().max());
        for (counter, _) in counters.iter().filter(|(_, value)| lost.contains(value)) {
            let class = match (min, max) {
                (_, None) => "lost-postfix",
                (_, Some(max)) if counter > max => "lost-postfix",
                (Some(min), _) if counter < min => "lost-prefix",
                _ => "lost-middle",
            };
            *classes.entry(class).or_default() += 1;
        }
    }
    classes
}

impl Checker<String, u64, Log> for Queue {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, history: &[Record<String, u64>], reads: &[Read<Log>]) -> Verdict {
        let attempted: BTreeSet<&String> = history
            .iter()
            .filter_map(|r| match &r.event {
                Event::Invoke { op } => Some(op),
                _ => None,
            })
            .collect();
        // Each acknowledged value and the sequences it was given.
        let mut acked: BTreeMap<&String, BTreeSet<u64>> = BTreeMap::new();
        for (op, record) in completions(history) {
            if let Event::Ok { value, .. } = &record.event {
                acked.entry(op).or_default().insert(*value);
            }
        }
        let unread: Vec<&str> = reads
            .iter()
            .filter(|r| r.result.is_err())
            .map(|r| r.node.as_str())
            .collect();
        let logs: Vec<(&str, &BTreeMap<u64, String>)> = reads
            .iter()
            .filter_map(|r| match &r.result {
                Ok(Log::Messages(messages)) => Some((r.node.as_str(), messages)),
                _ => None,
            })
            .collect();
        let missing_stream: Vec<&str> = reads
            .iter()
            .filter(|r| r.result == Ok(Log::StreamMissing))
            .map(|r| r.node.as_str())
            .collect();
        let held: Vec<BTreeSet<&String>> = logs
            .iter()
            .map(|(_, messages)| messages.values().collect())
            .collect();
        let union: BTreeSet<&String> = held.iter().flatten().copied().collect();
        let acked_values: BTreeSet<&String> = acked.keys().copied().collect();
        let lost: BTreeSet<&String> = acked_values.difference(&union).copied().collect();
        let unexpected: Vec<&String> = union
            .iter()
            .copied()
            .filter(|v| !attempted.contains(v))
            .collect();
        let divergent: BTreeSet<&String> = held
            .iter()
            .flat_map(|values| acked_values.iter().filter(|v| !values.contains(*v)))
            .copied()
            .filter(|v| !lost.contains(v))
            .collect();
        let mut at_seq: BTreeMap<u64, BTreeSet<&String>> = BTreeMap::new();
        for (_, messages) in &logs {
            for (seq, value) in messages.iter() {
                at_seq.entry(*seq).or_default().insert(value);
            }
        }
        let conflicts: Vec<u64> = at_seq
            .iter()
            .filter(|(_, values)| values.len() > 1)
            .map(|(seq, _)| *seq)
            .collect();
        let moved = acked
            .iter()
            .filter(|(value, _)| union.contains(*value))
            .filter(|(value, seqs)| {
                seqs.iter().any(|seq| {
                    logs.iter()
                        .any(|(_, m)| m.get(seq).is_some_and(|have| have != **value))
                })
            })
            .count();
        let mut by_seq: BTreeMap<u64, Vec<&String>> = BTreeMap::new();
        for (value, seqs) in &acked {
            for seq in seqs {
                by_seq.entry(*seq).or_default().push(value);
            }
        }
        // One value at two sequences of one node's log: stored twice.
        let mut duplicates: BTreeSet<&String> = BTreeSet::new();
        for (_, messages) in &logs {
            let mut seen = BTreeSet::new();
            duplicates.extend(messages.values().filter(|v| !seen.insert(*v)));
        }
        let reused: Vec<u64> = by_seq
            .iter()
            .filter(|(_, values)| values.len() > 1)
            .map(|(seq, _)| *seq)
            .collect();
        // A value no read node holds may be on a node that was never read.
        let lost_definite = unread.is_empty() && !lost.is_empty();
        let violation = lost_definite
            || !divergent.is_empty()
            || !unexpected.is_empty()
            || !conflicts.is_empty()
            || moved > 0
            || !reused.is_empty()
            || !duplicates.is_empty()
            || !missing_stream.is_empty();
        let lost_seqs = ranges(lost.iter().flat_map(|v| acked[*v].iter().copied()));
        // Details become property details, which keep 1024 serialized bytes:
        // counts first, then a few examples of each finding.
        let details = json!({
            "acked": acked_values.len(),
            "read": union.len(),
            "lost": lost.len(),
            "lost_classes": classify(&acked_values, &union, &lost),
            "lost_values": first(lost.iter().copied(), 5),
            "lost_seqs": lost_seqs.iter().take(5).collect::<Vec<_>>(),
            "divergent": divergent.len(),
            "divergent_values": first(divergent.iter().copied(), 5),
            "unexpected": unexpected.len(),
            "unexpected_values": first(unexpected.iter().copied(), 3),
            "seq_conflicts": ranges(conflicts.iter().copied()).into_iter().take(5).collect::<Vec<_>>(),
            "acked_seq_moved": moved,
            "acked_seq_reused": ranges(reused.iter().copied()).into_iter().take(5).collect::<Vec<_>>(),
            "duplicated": duplicates.len(),
            "duplicated_values": first(duplicates.iter().copied(), 3),
            "stream_missing": missing_stream.len(),
            "stream_missing_nodes": missing_stream.iter().take(5).collect::<Vec<_>>(),
            "unread": unread.len(),
            "unread_nodes": unread.iter().take(5).collect::<Vec<_>>(),
        });
        let (validity, summary) = if violation {
            let mut findings = Vec::new();
            for (count, what) in [
                (if lost_definite { lost.len() } else { 0 }, "lost"),
                (divergent.len(), "divergent"),
                (unexpected.len(), "unexpected"),
                (conflicts.len(), "sequence conflicts"),
                (moved, "moved"),
                (reused.len(), "reused sequences"),
                (duplicates.len(), "duplicated"),
                (missing_stream.len(), "nodes missing the stream"),
            ] {
                if count > 0 {
                    findings.push(format!("{count} {what}"));
                }
            }
            (
                Validity::Invalid,
                format!(
                    "{} of {} acknowledged publishes checked: {}",
                    acked_values.len(),
                    attempted.len(),
                    findings.join(", ")
                ),
            )
        } else if acked_values.is_empty() {
            (Validity::Unknown, "no publish was acknowledged".into())
        } else if !unread.is_empty() {
            let mut summary = format!("{} of {} nodes were never read", unread.len(), reads.len());
            if !lost.is_empty() {
                summary += &format!(
                    "; {} acknowledged publishes are on no node read",
                    lost.len()
                );
            }
            (Validity::Unknown, summary)
        } else {
            (
                Validity::Valid,
                format!(
                    "{} acknowledged publishes present on every node",
                    acked_values.len()
                ),
            )
        };
        Verdict {
            validity,
            summary,
            details,
        }
    }
}

#[cfg(test)]
mod tests;
