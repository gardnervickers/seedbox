//! Client-owned edits to recorded faults. Refreshes only add observations;
//! they never undo an edit or modify the server's retained inputs.
use seedbox_api::{SimulationId, TimelineEntry};
// Local document limits, independent of retained engine formats.
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub(super) struct TimelineMetadata {
    pub name: String,
    pub application: String,
    pub seed: Option<u64>,
    pub recorded_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct TimelineDraft {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    definition: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    working: Option<serde_json::Value>,
    source: SimulationId,
    created_at_unix_ms: u64,
    original: Vec<TimelineEntry>,
    excluded: BTreeSet<String>,
    undo: Option<BTreeSet<String>>,
    #[serde(default)]
    metadata: TimelineMetadata,
    #[serde(skip)]
    unsaved: bool,
    #[serde(skip)]
    save_error: Option<String>,
    /// Whether this draft was loaded from or written to its file. A draft that
    /// was neither never replaces an existing file it could not read.
    #[serde(skip)]
    persisted: bool,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl TimelineDraft {
    pub(super) fn new(source: SimulationId, created_at_unix_ms: u64) -> Self {
        Self {
            version: 2,
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
            unsaved: true,
            save_error: None,
            persisted: false,
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
                        self.unsaved = true;
                    }
                }
                base["spec"]["overrides"] = overrides.into();
            }
            self.edit_definition();
        } else {
            if self.definition.as_ref() != Some(&document) {
                self.unsaved = true;
            }
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

    /// Explicit save publishes a new immutable full definition. Edits continue
    /// in the same working copy; an existing saved file is never replaced.
    pub(super) fn save_definition(&mut self, directory: &Path) -> io::Result<PathBuf> {
        let document = self
            .submission_snapshot()
            .ok_or_else(|| invalid("complete prepared definition unavailable"))?;
        let bytes = serde_json::to_vec_pretty(&document).map_err(|e| invalid(e.to_string()))?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("saved definition exceeds size limit"));
        }
        self.save(directory)?;
        let snapshots = directory.join("definitions");
        fs::create_dir_all(&snapshots)?;
        let id =
            seedbox_client::Client::request_id().map_err(|e| io::Error::other(e.to_string()))?;
        let path = snapshots.join(format!("{}-{id}.json", self.source));
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        if let Err(error) = file.write_all(&bytes) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(path)
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
        self.unsaved = true;
        true
    }
    pub(super) fn rename(&mut self, name: String) {
        self.metadata.name = name;
        self.edit_definition();
        self.unsaved = true;
    }
    pub(super) fn is_unsaved(&self) -> bool {
        self.unsaved
    }
    pub(super) fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }
    fn fill_legacy_metadata(&mut self) {
        if self.metadata.name.is_empty() {
            self.metadata.name = self.source.to_string();
        }
        if self.metadata.recorded_at_unix_ms == 0 {
            self.metadata.recorded_at_unix_ms = self.created_at_unix_ms;
        }
    }

    pub(super) fn source(&self) -> &SimulationId {
        &self.source
    }
    pub(super) fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    /// Load saved recordings even when their server catalogue is gone. Files
    /// that cannot be read are skipped and described, never replaced.
    pub(super) fn load_all(directory: &Path) -> io::Result<(Vec<Self>, Vec<String>)> {
        let files = match fs::read_dir(directory) {
            Ok(files) => files,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()))
            }
            Err(error) => return Err(error),
        };
        let mut drafts = Vec::new();
        let mut skipped = Vec::new();
        for file in files {
            let path = match file {
                Ok(file) => file.path(),
                Err(error) => {
                    skipped.push(format!("{}: {error}", directory.display()));
                    continue;
                }
            };
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            match Self::load(directory, &path) {
                Ok(draft) => drafts.push(draft),
                Err(error) => skipped.push(format!("{}: {error}", path.display())),
            }
        }
        drafts.sort_by(|left, right| {
            right
                .created_at_unix_ms
                .cmp(&left.created_at_unix_ms)
                .then_with(|| left.source.cmp(&right.source))
        });
        Ok((drafts, skipped))
    }

    fn load(directory: &Path, path: &Path) -> io::Result<Self> {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("saved timeline exceeds size limit"));
        }
        let mut draft = Self::decode(&bytes).map_err(|error| invalid(error.to_string()))?;
        draft.fill_legacy_metadata();
        draft.validate()?;
        if draft.path(directory) != path {
            return Err(invalid("saved timeline filename disagrees with its source"));
        }
        draft.persisted = true;
        Ok(draft)
    }

    pub(super) fn path(&self, directory: &Path) -> PathBuf {
        directory.join(format!("{}-{}.json", self.source, self.created_at_unix_ms))
    }

    fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let mut document: serde_json::Value = serde_json::from_slice(bytes)?;
        if document["version"] == 1 {
            let mut entries = Vec::new();
            for old in document["original"].as_array().into_iter().flatten() {
                let plan = &old["plan"];
                let mut action = plan.clone();
                if let Some(fields) = action.as_object_mut() {
                    fields.remove("id");
                    fields.remove("at_ns");
                    fields.remove("deadline_ns");
                }
                entries.push(serde_json::json!({"id":format!("recorded:{}",old["requestId"].as_str().unwrap_or_default()),"atNs":plan["at_ns"],"action":action,"origin":"recorded","position":old["position"],"faultId":plan["id"]}));
            }
            for plan in document["scheduled"].as_array().into_iter().flatten() {
                let mut action = plan.clone();
                if let Some(fields) = action.as_object_mut() {
                    fields.remove("id");
                    fields.remove("at_ns");
                    fields.remove("deadline_ns");
                }
                entries.push(serde_json::json!({"id":format!("program:input-{}",plan["id"]),"atNs":plan["at_ns"],"action":action,"origin":"program","faultId":plan["id"]}));
            }
            for field in ["excluded", "undo"] {
                if let Some(ids) = document[field].as_array_mut() {
                    for id in ids {
                        if let Some(old) = id.as_str() {
                            let replacement = old
                                .strip_prefix("scheduled:")
                                .map(|suffix| format!("program:input-{suffix}"))
                                .unwrap_or_else(|| format!("recorded:{old}"));
                            *id = replacement.into();
                        }
                    }
                }
            }
            document["version"] = 2.into();
            document["original"] = entries.into();
            document.as_object_mut().unwrap().remove("scheduled");
        }
        serde_json::from_value(document)
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != 2 {
            return Err(invalid("unsupported saved timeline version"));
        }
        if self.original.len() > MAX_ENTRIES {
            return Err(invalid("timeline exceeds entry limit"));
        }
        let mut ids = BTreeSet::new();
        for entry in &self.original {
            if entry.id.is_empty() || !ids.insert(&entry.id) {
                return Err(invalid("timeline entries need unique nonempty IDs"));
            }
        }
        let entries = self.entries();
        for excluded in std::iter::once(&self.excluded).chain(self.undo.iter()) {
            if excluded
                .iter()
                .any(|id| !entries.iter().any(|entry| &entry.id == id))
            {
                return Err(invalid("edited timeline excludes an unknown fault"));
            }
        }
        Ok(())
    }

    pub(super) fn save(&mut self, directory: &Path) -> io::Result<()> {
        let result = self.write(directory);
        match &result {
            Ok(()) => {
                self.unsaved = false;
                self.save_error = None;
                self.persisted = true;
            }
            Err(error) => {
                self.unsaved = true;
                self.save_error = Some(error.to_string());
            }
        }
        result
    }
    pub(super) fn retry_save(&mut self, directory: &Path) -> io::Result<()> {
        self.save(directory)
    }
    fn write(&self, directory: &Path) -> io::Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("saved timeline exceeds size limit"));
        }
        fs::create_dir_all(directory)?;
        let path = self.path(directory);
        if !self.persisted && path.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{}: an unreadable saved timeline is in the way; move it aside to save",
                    path.display()
                ),
            ));
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
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
        next.unsaved |= changed;
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
        self.unsaved = true;
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
        self.unsaved = true;
        true
    }

    pub(super) fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.take() else {
            return false;
        };
        self.excluded = previous;
        self.edit_definition();
        self.unsaved = true;
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
    fn one_working_copy_saves_immutable_full_definitions_and_snapshots_submission() {
        let directory = std::env::temp_dir().join(format!(
            "seedbox-definition-{}",
            seedbox_client::Client::request_id().unwrap()
        ));
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
        let first = draft.save_definition(&directory).unwrap();
        let saved_bytes = fs::read(&first).unwrap();
        draft.toggle(1);
        assert_eq!(submitted["spec"]["overrides"].as_array().unwrap().len(), 1);
        assert_eq!(fs::read(&first).unwrap(), saved_bytes);
        let second = draft.save_definition(&directory).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), saved_bytes);
        let (restored, _) = TimelineDraft::load_all(&directory).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(
            restored[0].submission_snapshot().unwrap()["spec"]["overrides"],
            serde_json::json!([])
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unknown_actions_survive_edits_refresh_and_roundtrip() {
        let mut draft = draft();
        draft.merge(&[entry(1), entry(2)]).unwrap();
        draft.toggle(0);
        draft.merge(&[entry(1), entry(2), entry(3)]).unwrap();
        assert!(!draft.included_entry(&entry(1)));
        assert!(draft.included_entry(&entry(3)));
        let bytes = serde_json::to_vec(&draft).unwrap();
        let restored = TimelineDraft::decode(&bytes).unwrap();
        assert_eq!(restored.entries(), draft.entries());
        assert!(!restored.included_entry(&entry(1)));
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
    fn persistence_and_failed_save_retry_preserve_unknown_actions_and_metadata() {
        let directory = std::env::temp_dir().join(format!(
            "seedbox-timeline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&directory, b"blocked").unwrap();
        let mut draft = draft();
        draft.merge(&[entry(1)]).unwrap();
        draft.toggle(0);
        draft.set_metadata(TimelineMetadata {
            name: "Failure".into(),
            application: "test".into(),
            seed: Some(42),
            recorded_at_unix_ms: 123,
        });
        assert!(draft.save(&directory).is_err());
        assert!(draft.is_unsaved());
        assert!(draft.save_error().is_some());
        fs::remove_file(&directory).unwrap();
        draft.retry_save(&directory).unwrap();
        assert!(!draft.is_unsaved());
        assert!(draft.save_error().is_none());
        let restored = TimelineDraft::load_all(&directory).unwrap().0.remove(0);
        assert_eq!(restored.entries(), draft.entries());
        assert_eq!(restored.metadata(), draft.metadata());
        assert_eq!(restored.modified_count(), 1);
        fs::write(draft.path(&directory), b"broken").unwrap();
        let (loaded, skipped) = TimelineDraft::load_all(&directory).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(skipped.len(), 1);
        let mut fresh = TimelineDraft::new("k7m2p9qx".parse().unwrap(), 123);
        assert!(fresh.save(&directory).is_err());
        assert!(fresh.save_error().unwrap().contains("move it aside"));
        assert_eq!(fs::read(draft.path(&directory)).unwrap(), b"broken");
        fs::remove_dir_all(directory).unwrap();
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
    #[test]
    fn legacy_saved_actions_migrate_without_engine_decoding() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version":1,"source":"k7m2p9qx","createdAtUnixMs":123,
            "original":[{"requestId":"fault-one","position":{"boundaryIndex":4,"completedRounds":3,"simulatedTimeNs":100},
                "plan":{"id":1,"at_ns":100,"deadline_ns":200,"action":"future_action","unknown":true}}],
            "excluded":["fault-one"],"undo":null,"scheduled":[]
        })).unwrap();
        let draft = TimelineDraft::decode(&bytes).unwrap();
        draft.validate().unwrap();
        assert_eq!(draft.entries()[0].id, "recorded:fault-one");
        assert_eq!(
            draft.entries()[0].action,
            serde_json::json!({"action":"future_action","unknown":true})
        );
        assert!(!draft.included_entry(&draft.entries()[0]));
    }
}
