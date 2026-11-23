//! What the oracles compare beyond the server's own comparison: output,
//! engine events, fault identity and the timeline.
use crate::session::{err, Session};
use crate::Outcome;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub machine: String,
    pub incarnation: u32,
    pub time: u64,
    pub stream: String,
    pub bytes: Vec<u8>,
}

type Streams = BTreeMap<(String, u32, String), Vec<u8>>;

/// Each machine stream's bytes; record boundaries are an observation detail.
pub fn by_machine(records: &[Record]) -> Streams {
    let mut streams = Streams::new();
    for r in records {
        streams
            .entry((r.machine.clone(), r.incarnation, r.stream.clone()))
            .or_default()
            .extend_from_slice(&r.bytes);
    }
    streams
}

/// Records committed at or after `at`.
pub fn suffix(records: Vec<Record>, at: u64) -> Vec<Record> {
    records.into_iter().filter(|r| r.time >= at).collect()
}

/// Equal per-stream output, or where the first difference is.
pub fn same_output(expected: &[Record], actual: &[Record]) -> Outcome {
    let (expected, actual) = (by_machine(expected), by_machine(actual));
    if expected == actual {
        return Ok(());
    }
    for key in expected.keys().chain(actual.keys()) {
        let (a, b) = (
            expected.get(key).cloned().unwrap_or_default(),
            actual.get(key).cloned().unwrap_or_default(),
        );
        if a != b {
            let at = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
            let show = |v: &[u8]| {
                String::from_utf8_lossy(
                    &v[at.saturating_sub(80).min(v.len())..(at + 160).min(v.len())],
                )
                .into_owned()
            };
            return Err(format!(
                "{key:?} at byte {at} (lengths {} and {}):\nexpected: {}\nactual:   {}",
                a.len(),
                b.len(),
                show(&a),
                show(&b)
            ));
        }
    }
    unreachable!("unequal maps have a differing key")
}

/// Fault identity without mutable status: ID, input key, action, plan and time.
pub fn fault_identity(faults: &[seedbox_api::FaultState]) -> Vec<Value> {
    faults
        .iter()
        .map(|f| {
            json!({"id": f.id, "inputKey": f.input_key, "action": f.action, "plan": f.plan,
                   "appliedTimeNs": f.applied_time_ns})
        })
        .collect()
}

/// Faults applied at or after `at`.
pub fn later_faults(faults: &[seedbox_api::FaultState], at: u64) -> Vec<Value> {
    fault_identity(faults)
        .into_iter()
        .filter(|f| f["appliedTimeNs"].as_u64().is_some_and(|t| t >= at))
        .collect()
}

/// Everything about one run that a prefix comparison inspects.
pub struct Trace {
    pub logs: Vec<Record>,
    pub events: Vec<(u64, Value)>,
    pub faults: Vec<Value>,
    pub timeline: Vec<seedbox_api::TimelineEntry>,
}

impl Trace {
    pub async fn read(session: &Session, id: &seedbox_api::SimulationId) -> Result<Self, String> {
        Ok(Self {
            logs: session.logs(id).await?,
            events: session.events(id).await?,
            faults: fault_identity(&session.client.world(id).await.map_err(err)?.faults),
            timeline: session.client.timeline(id).await.map_err(err)?.entries,
        })
    }

    /// Faults the oracle itself requested (no external input key).
    pub fn oracle_actions(&self) -> Vec<Value> {
        self.faults
            .iter()
            .filter(|f| f["inputKey"].is_null())
            .cloned()
            .collect()
    }

    /// Everything observed is identical, including log record boundaries.
    pub fn same(&self, other: &Self) -> Outcome {
        self.same_prefix(other, u64::MAX)?;
        if self.logs != other.logs {
            return Err("log records differ in their boundaries or times".into());
        }
        Ok(())
    }

    pub fn same_prefix(&self, other: &Self, before: u64) -> Outcome {
        let logs = |t: &Self| -> Vec<Record> {
            t.logs.iter().filter(|r| r.time < before).cloned().collect()
        };
        same_output(&logs(self), &logs(other)).map_err(|e| format!("output differs:\n{e}"))?;
        let events = |t: &Self| -> Vec<(u64, Value)> {
            t.events
                .iter()
                .filter(|(time, _)| *time < before)
                .cloned()
                .collect()
        };
        let (mine, theirs) = (events(self), events(other));
        if mine != theirs {
            let at = mine.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
            return Err(format!(
                "events differ at index {at} of {} and {}: {:?} vs {:?}",
                mine.len(),
                theirs.len(),
                mine.get(at),
                theirs.get(at)
            ));
        }
        let faults = |t: &Self| -> Vec<Value> {
            t.faults
                .iter()
                .filter(|f| {
                    f["appliedTimeNs"]
                        .as_u64()
                        .is_some_and(|time| time < before)
                })
                .cloned()
                .collect()
        };
        if faults(self) != faults(other) {
            return Err(format!(
                "faults differ: {:?} vs {:?}",
                faults(self),
                faults(other)
            ));
        }
        let timeline = |t: &Self| -> Vec<seedbox_api::TimelineEntry> {
            t.timeline
                .iter()
                .filter(|e| e.at_ns < before)
                .cloned()
                .collect()
        };
        if timeline(self) != timeline(other) {
            return Err(format!(
                "timeline differs: {:?} vs {:?}",
                timeline(self),
                timeline(other)
            ));
        }
        Ok(())
    }
}
