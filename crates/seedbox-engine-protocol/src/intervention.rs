//! Retained semantic inputs for interactive faults. Host arrival times and
//! delivery sequences deliberately do not participate in this history.

use crate::control::Plan;
use crate::host_control::BoundaryPosition;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const VERSION: u32 = 1;
pub const MAX_ENTRIES: usize = 4096;
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ENTRY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Intervention {
    pub request_id: String,
    pub position: BoundaryPosition,
    /// The exact admitted plan, including the assigned fault ID. Gaps between
    /// IDs are legal: scenario and guest controls share the admission sequence.
    pub plan: Plan,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct History {
    pub version: u32,
    pub entries: Vec<Intervention>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: Vec::new(),
        }
    }
}

impl History {
    /// Validate the retained format and ordering. Machine/handle availability
    /// and action parameters are still admitted by the world at this position.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!(
                "unsupported intervention history version {}",
                self.version
            ));
        }
        if self.entries.len() > MAX_ENTRIES {
            return Err(format!(
                "intervention history exceeds {MAX_ENTRIES} entries"
            ));
        }
        let mut requests = HashSet::new();
        let mut previous: Option<&Intervention> = None;
        for entry in &self.entries {
            let id = &entry.request_id;
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err("intervention request ID must be 1-128 ASCII letters, digits, hyphens or underscores".into());
            }
            if !requests.insert(id) {
                return Err(format!("duplicate intervention request ID {id}"));
            }
            if entry.position.boundary_index == 0 || entry.plan.id == 0 {
                return Err(format!(
                    "intervention {id} needs nonzero boundary and fault IDs"
                ));
            }
            if entry.plan.at_ns < entry.position.simulated_time_ns
                || entry.plan.deadline_ns < entry.plan.at_ns
            {
                return Err(format!(
                    "intervention {id} has an invalid effect time or deadline"
                ));
            }
            if let Some(prior) = previous {
                if entry.plan.id <= prior.plan.id
                    || entry.position.boundary_index < prior.position.boundary_index
                    || entry.position.completed_rounds < prior.position.completed_rounds
                    || (entry.position.boundary_index == prior.position.boundary_index
                        && entry.position != prior.position)
                {
                    return Err(format!("intervention {id} is out of admission order"));
                }
            }
            if serde_json::to_vec(entry).map_err(|e| e.to_string())?.len() > MAX_ENTRY_BYTES {
                return Err(format!("intervention {id} exceeds {MAX_ENTRY_BYTES} bytes"));
            }
            previous = Some(entry);
        }
        Ok(())
    }

    /// Canonical compact storage encoding, bounded before it is written.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err(format!("intervention history exceeds {MAX_BYTES} bytes"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err(format!("intervention history exceeds {MAX_BYTES} bytes"));
        }
        let history: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        history.validate()?;
        Ok(history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(request: &str, fault: u32, boundary: u64, time: u64) -> Intervention {
        Intervention {
            request_id: request.into(),
            position: BoundaryPosition {
                boundary_index: boundary,
                completed_rounds: 5,
                simulated_time_ns: time,
            },
            plan: serde_json::from_value(serde_json::json!({
                "id":fault,"at_ns":time,"deadline_ns":time,
                "action":"heal","handle":"network"
            }))
            .unwrap(),
        }
    }

    #[test]
    fn admission_order_allows_id_gaps_and_segment_time_rewinds() {
        let history = History {
            entries: vec![
                entry("a", 2, 8, 100),
                entry("b", 4, 8, 100),
                entry("c", 7, 9, 99),
            ],
            ..History::default()
        };
        assert_eq!(
            History::decode(&history.encode().unwrap()).unwrap(),
            history
        );
        for mutation in 0..7 {
            let mut bad = history.clone();
            match mutation {
                0 => bad.entries[1].request_id = "a".into(),
                1 => bad.entries[1].plan.id = 2,
                2 => bad.entries[1].position.simulated_time_ns = 99,
                3 => bad.entries[2].position.boundary_index = 7,
                4 => bad.entries[2].position.completed_rounds = 4,
                5 => bad.entries[0].plan.at_ns = 99,
                _ => bad.entries[0].plan.deadline_ns = 99,
            }
            assert!(bad.validate().is_err(), "accepted mutation {mutation}");
        }
    }
}
