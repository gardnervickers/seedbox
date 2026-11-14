//! Client-owned, in-memory edits to recorded faults. Refreshes only add
//! observations; they never undo an edit or modify the server's retained
//! inputs. Nothing here is written to disk.
use seedbox_api::{SimulationId, TimelineEntry};
const MAX_ENTRIES: usize = 4096;
use std::{collections::BTreeSet, io};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct TimelineMetadata {
    pub name: String,
    pub application: String,
    pub seed: Option<u64>,
    pub recorded_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
pub(super) struct TimelineDraft {
    definition: Option<serde_json::Value>,
    working: Option<serde_json::Value>,
    source: SimulationId,
    created_at_unix_ms: u64,
    original: Vec<TimelineEntry>,
    excluded: BTreeSet<String>,
    undo: Option<BTreeSet<String>>,
    metadata: TimelineMetadata,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl TimelineDraft {
    pub(super) fn new(source: SimulationId, created_at_unix_ms: u64) -> Self {
        Self {
            definition: None,
            working: None,
            source: source.clone(),
            created_at_unix_ms,
            original: Vec::new(),
            excluded: BTreeSet::new(),
            undo: None,
            metadata: TimelineMetadata {
                name: source.to_string(),
                recorded_at_unix_ms: created_at_unix_ms,
                ..TimelineMetadata::default()
            },
        }
    }

    /// Observations refresh the immutable base only until editing begins.
    pub(super) fn attach_definition(&mut self, resource: &seedbox_api::Simulation) {
        let Ok(mut document) = serde_json::to_value(resource) else {
            return;
        };
        if document["spec"].get("architecture").is_none() {
            return;
        }
        let Some(fields) = document.as_object_mut() else {
            return;
        };
        fields.remove("status");
        fields.remove("options");
        let mut metadata = serde_json::Map::new();
        for key in ["name", "labels", "annotations"] {
            if let Some(value) = fields["metadata"].get(key) {
                metadata.insert(key.into(), value.clone());
            }
        }
        fields.insert("metadata".into(), metadata.into());
        if self.working.is_some() {
            // A later external injection enriches this experiment's overrides.
            // Preserve every local edit and all existing resolved settings.
            let incoming = document["spec"]["overrides"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if let Some(base) = &mut self.definition {
                let mut overrides = base["spec"]["overrides"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                for entry in incoming {
                    if !overrides.iter().any(|old| old["id"] == entry["id"]) {
                        overrides.push(entry);
                    }
                }
                base["spec"]["overrides"] = overrides.into();
            }
            self.edit_definition();
        } else {
            self.definition = Some(document);
        }
    }

    fn edit_definition(&mut self) {
        if self.working.is_none() {
            self.working = self.definition.clone();
        }
        let Some(document) = &mut self.working else {
            return;
        };
        // Oracle observations are evidence. Only authored overrides are edited.
        let overrides = self
            .definition
            .as_ref()
            .and_then(|base| base["spec"]["overrides"].as_array())
            .cloned()
            .unwrap_or_default();
        document["spec"]["overrides"] = overrides
            .into_iter()
            .filter(|entry| {
                let id = entry["id"].as_str().unwrap_or_default();
                !self.excluded.contains(id) && !self.excluded.contains(&format!("program:{id}"))
            })
            .collect::<Vec<_>>()
            .into();
        document["metadata"]["name"] = self.metadata.name.clone().into();
    }

    pub(super) fn submission_snapshot(&self) -> Option<serde_json::Value> {
        self.working.as_ref().or(self.definition.as_ref()).cloned()
    }

    pub(super) fn metadata(&self) -> &TimelineMetadata {
        &self.metadata
    }
    pub(super) fn set_metadata(&mut self, metadata: TimelineMetadata) -> bool {
        if self.metadata == metadata {
            return false;
        }
        self.metadata = metadata;
        if self.working.is_some() {
            self.edit_definition();
        }
        true
    }
    pub(super) fn rename(&mut self, name: String) {
        self.metadata.name = name;
        self.edit_definition();
    }
    pub(super) fn source(&self) -> &SimulationId {
        &self.source
    }
    pub(super) fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    fn validate(&self) -> io::Result<()> {
        if self.original.len() > MAX_ENTRIES {
            return Err(invalid("timeline exceeds entry limit"));
        }
        let mut ids = BTreeSet::new();
        for entry in &self.original {
            if entry.id.is_empty() || !ids.insert(&entry.id) {
                return Err(invalid("timeline entries need unique nonempty IDs"));
            }
        }
        Ok(())
    }

    /// Merge a full or partial server observation by stable request identity.
    pub(super) fn merge(&mut self, history: &[TimelineEntry]) -> io::Result<bool> {
        let mut next = self.clone();
        for entry in history {
            if let Some(previous) = next
                .original
                .iter_mut()
                .find(|previous| previous.id == entry.id)
            {
                if previous.at_ns != entry.at_ns
                    || previous.action != entry.action
                    || previous.origin != entry.origin
                    || previous.position != entry.position
                {
                    return Err(invalid("recorded fault changed for an existing entry ID"));
                }
                if entry.fault_id.is_some() {
                    previous.fault_id = entry.fault_id;
                }
            } else {
                next.original.push(entry.clone());
            }
        }
        next.original.sort_by_key(|entry| entry.at_ns);
        next.validate()?;
        let changed = next.original != self.original;
        *self = next;
        Ok(changed)
    }

    pub(super) fn entries(&self) -> &[TimelineEntry] {
        &self.original
    }
    pub(super) fn next_upcoming(&self, world: &seedbox_api::WorldState) -> Option<usize> {
        self.entries().iter().position(|entry| {
            match world
                .faults
                .iter()
                .find(|fault| Some(fault.id) == entry.fault_id)
            {
                None => true,
                Some(fault) if fault.plan.as_ref() == Some(&entry.action) => matches!(
                    fault.status,
                    seedbox_api::FaultStatus::Unrequested | seedbox_api::FaultStatus::Requested
                ),
                Some(_) => false,
            }
        })
    }

    pub(super) fn included_entry(&self, entry: &TimelineEntry) -> bool {
        !self.excluded.contains(&entry.id)
    }

    pub(super) fn modified_count(&self) -> usize {
        self.excluded.len()
    }

    pub(super) fn toggle(&mut self, index: usize) -> bool {
        let Some(entry) = self.original.get(index) else {
            return false;
        };
        if self.definition.is_some() && entry.origin == seedbox_api::TimelineOrigin::Recorded {
            return false;
        }
        self.undo = Some(self.excluded.clone());
        let id = entry.id.clone();
        if !self.excluded.remove(&id) {
            self.excluded.insert(id);
        }
        self.edit_definition();
        true
    }

    /// Excludes only faults already observed, so future injections stay visible.
    pub(super) fn clear(&mut self) -> bool {
        let excluded = self
            .entries()
            .iter()
            .filter(|entry| {
                self.definition.is_none() || entry.origin != seedbox_api::TimelineOrigin::Recorded
            })
            .map(|entry| entry.id.clone())
            .collect();
        self.set_excluded(excluded)
    }

    pub(super) fn reset(&mut self) -> bool {
        self.set_excluded(BTreeSet::new())
    }

    fn set_excluded(&mut self, excluded: BTreeSet<String>) -> bool {
        if excluded == self.excluded {
            return false;
        }
        self.undo = Some(std::mem::replace(&mut self.excluded, excluded));
        self.edit_definition();
        true
    }

    pub(super) fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.take() else {
            return false;
        };
        self.excluded = previous;
        self.edit_definition();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: u32) -> TimelineEntry {
        TimelineEntry {
            id: format!("entry-{id}"),
            at_ns: u64::from(id),
            action: serde_json::json!({"action":"future_fault","novel":{"parameter":id}}),
            origin: seedbox_api::TimelineOrigin::Program,
            position: None,
            fault_id: Some(id),
        }
    }
    fn draft() -> TimelineDraft {
        TimelineDraft::new("k7m2p9qx".parse().unwrap(), 123)
    }
    #[test]
    fn working_copy_snapshots_submission_without_touching_base() {
        let mut draft = draft();
        let base = serde_json::json!({"apiVersion":"seedbox/v1", "kind":"Simulation", "metadata":{"name":"test"},
            "spec":{"architecture":"aarch64","overrides":[
                {"id":"entry-1","atNs":1,"action":{"action":"future"}},
                {"id":"entry-2","atNs":2,"action":{"action":"future"}}]}});
        draft.definition = Some(base.clone());
        draft.merge(&[entry(1), entry(2)]).unwrap();
        assert!(draft.working.is_none());
        draft.toggle(0);
        assert_eq!(draft.definition.as_ref(), Some(&base));
        draft.rename("reproduction".into());
        let submitted = draft.submission_snapshot().unwrap();
        assert_eq!(submitted["metadata"]["name"], "reproduction");
        assert_eq!(submitted["spec"]["overrides"].as_array().unwrap().len(), 1);
        draft.toggle(1);
        assert_eq!(submitted["spec"]["overrides"].as_array().unwrap().len(), 1);
        assert_eq!(
            draft.submission_snapshot().unwrap()["spec"]["overrides"],
            serde_json::json!([])
        );
    }

    #[test]
    fn unknown_actions_survive_edits_and_refresh() {
        let mut draft = draft();
        draft.merge(&[entry(1), entry(2)]).unwrap();
        draft.toggle(0);
        draft.merge(&[entry(1), entry(2), entry(3)]).unwrap();
        assert!(!draft.included_entry(&entry(1)));
        assert!(draft.included_entry(&entry(3)));
        assert!(draft.clear());
        assert!(draft.undo());
        assert!(draft.reset());
        assert_eq!(draft.modified_count(), 0);
    }
    #[test]
    fn conflicting_observation_is_transactional() {
        let mut draft = draft();
        draft.merge(&[entry(1)]).unwrap();
        let mut changed = entry(1);
        changed.action["new"] = true.into();
        assert!(draft.merge(&[entry(2), changed]).is_err());
        assert_eq!(draft.entries(), &[entry(1)]);
    }
    #[test]
    fn observation_enrichment_retains_removals_and_new_fault_identity() {
        let mut draft = draft();
        let mut pending = entry(1);
        pending.fault_id = None;
        draft.merge(&[pending]).unwrap();
        draft.toggle(0);
        assert!(draft.merge(&[entry(1)]).unwrap());
        assert_eq!(draft.entries()[0].fault_id, Some(1));
        assert!(!draft.included_entry(&draft.entries()[0]));
    }
}
