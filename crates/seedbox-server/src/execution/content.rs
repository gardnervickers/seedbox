//! Reclaim the cache objects that only deleted resources referenced.
//!
//! Captured checkpoint state and imported binaries are owned cache objects
//! (`ContentStore::put_*_owned`). After a deletion, every owned object that no
//! remaining resource, retained input file, checkpoint exported from a remaining
//! source or unclaimed checkpoint upload names is removed. Reusable inputs
//! (kernels, prepared images and guests) are never owned, so this is not cache
//! pruning. Work that creates an owned object holds `references` for reading
//! until a resource or one of those registries names it; reclamation holds it
//! for writing, after the evidence lock that orders imports, exports and deletes.
//! The catalogue remembers what a deletion reclaimed, so a later submission
//! that names it fails at admission; an upload or export that puts the content
//! back clears that record.
use super::{Error, ExecutionService};
use crate::catalogue;
use seedbox_api::SimulationId;
use seedbox_application::store::ContentDigest;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

/// Content an export or an upload pinned until a resource names it.
#[derive(Default)]
pub(super) struct Pins {
    /// Captured state of exported checkpoints, by source and checkpoint: a
    /// fetched branch stays submittable while its source remains.
    exports: BTreeMap<SimulationId, BTreeMap<String, BTreeSet<ContentDigest>>>,
    /// Uploaded checkpoints, newest first, with the resources that existed at
    /// upload. Each lasts until a later resource that names all its content
    /// claims it.
    uploads: Vec<(BTreeSet<SimulationId>, BTreeSet<ContentDigest>)>,
    /// Remaining resources that claimed an upload. Each claims at most one, so
    /// a second upload of the same checkpoint stays until its own submission.
    claimed: BTreeSet<SimulationId>,
    /// The latest exported checkpoint that named each digest, for the error a
    /// submission gets after deletion reclaims it.
    exported: BTreeMap<ContentDigest, (SimulationId, String)>,
}

/// Every digest a JSON document names, as `sha256:<hex>` or `<hex>`. Roots may
/// over-approximate: an extra digest only keeps an object longer.
pub(crate) fn digests_in(value: &serde_json::Value, digests: &mut BTreeSet<ContentDigest>) {
    fn add(text: &str, digests: &mut BTreeSet<ContentDigest>) {
        let hex = text.strip_prefix("sha256:").unwrap_or(text);
        if let Ok(digest) = ContentDigest::try_from(hex.to_owned()) {
            digests.insert(digest);
        }
    }
    match value {
        serde_json::Value::String(text) => add(text, digests),
        serde_json::Value::Array(values) => {
            for value in values {
                digests_in(value, digests);
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                add(key, digests);
                digests_in(value, digests);
            }
        }
        _ => {}
    }
}

fn digests_of(value: &impl serde::Serialize) -> Result<BTreeSet<ContentDigest>, Error> {
    let mut digests = BTreeSet::new();
    digests_in(
        &serde_json::to_value(value).map_err(std::io::Error::other)?,
        &mut digests,
    );
    Ok(digests)
}

impl ExecutionService {
    /// Remove owned content that no remaining resource references. Deletion
    /// does this itself; the server also calls it before a graceful shutdown,
    /// after which still referenced objects remain as ordinary cache content.
    pub fn reclaim_content(&self) -> Result<Vec<ContentDigest>, Error> {
        let _evidence = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        self.reclaim_unreferenced(None)
    }

    /// The content each resource (or only `only`) names in the catalogue and
    /// its retained input file.
    pub(super) fn resource_digests(
        &self,
        only: Option<&SimulationId>,
    ) -> Result<BTreeMap<SimulationId, BTreeSet<ContentDigest>>, Error> {
        let mut resources: BTreeMap<SimulationId, BTreeSet<ContentDigest>> = BTreeMap::new();
        for (id, value) in self.manager.content()? {
            if only.is_none_or(|only| *only == id) {
                digests_in(&value, resources.entry(id).or_default());
            }
        }
        for (id, digests) in &mut resources {
            match fs::read(self.root.join("inputs").join(format!("{id}.json"))) {
                Ok(bytes) => digests_in(
                    &serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
                    digests,
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(resources)
    }

    /// Callers hold the evidence lock. `deleted` is the resource just deleted
    /// with the content it named: it can still claim the upload it was
    /// submitted from, and the catalogue records what its deletion reclaimed.
    pub(super) fn reclaim_unreferenced(
        &self,
        deleted: Option<(&SimulationId, BTreeSet<ContentDigest>)>,
    ) -> Result<Vec<ContentDigest>, Error> {
        let _references = self
            .references
            .write()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let resources = self.resource_digests(None)?;
        let mut roots: BTreeSet<ContentDigest> = resources.values().flatten().cloned().collect();
        let mut pins = self
            .pins
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        pins.exports
            .retain(|source, _| resources.contains_key(source));
        // A resource later than an upload is later than every older one too,
        // so newer uploads are claimed first; the deleted resource claims
        // before the remaining ones, which can still claim later.
        let named: Vec<_> = deleted
            .as_ref()
            .map(|(id, digests)| (*id, digests))
            .into_iter()
            .chain(&resources)
            .collect();
        let Pins {
            uploads, claimed, ..
        } = &mut *pins;
        uploads.retain(|(existing, digests)| {
            let claimant = named.iter().find(|(id, named)| {
                !existing.contains(*id) && !claimed.contains(*id) && digests.is_subset(named)
            });
            if let Some((id, _)) = claimant {
                claimed.insert((*id).clone());
            }
            claimant.is_none()
        });
        claimed.retain(|id| resources.contains_key(id));
        roots.extend(
            pins.exports
                .values()
                .flat_map(|captures| captures.values().flatten())
                .cloned(),
        );
        roots.extend(
            pins.uploads
                .iter()
                .flat_map(|(_, digests)| digests)
                .cloned(),
        );
        let removed = self.store.reclaim_owned(&roots)?;
        let records: Vec<_> = removed
            .iter()
            .filter_map(|digest| {
                let checkpoint = pins.exported.remove(digest);
                let deleted = deleted.as_ref()?.0.clone();
                let checkpoint = checkpoint
                    .map(|(source, name)| format!("checkpoint {name} of simulation {source}"));
                Some((
                    digest.clone(),
                    catalogue::Reclaimed {
                        checkpoint,
                        deleted,
                    },
                ))
            })
            .collect();
        drop(pins);
        self.manager.reclaimed(records)?;
        Ok(removed)
    }

    /// Callers hold `references` for reading from the capture.
    pub(super) fn pin_export(
        &self,
        id: &SimulationId,
        name: &str,
        start: &seedbox_spec::definition::Start,
    ) -> Result<(), Error> {
        let digests = digests_of(start)?;
        self.manager.restored(&digests)?;
        let mut pins = self
            .pins
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        for digest in &digests {
            pins.exported
                .insert(digest.clone(), (id.clone(), name.to_owned()));
        }
        pins.exports
            .entry(id.clone())
            .or_default()
            .insert(name.to_owned(), digests);
        Ok(())
    }

    /// Callers hold `references` for reading from the import.
    pub(super) fn pin_upload(
        &self,
        definition: &seedbox_spec::definition::Definition,
    ) -> Result<(), Error> {
        let digests = digests_of(definition)?;
        self.manager.restored(&digests)?;
        let existing = self
            .manager
            .read(|catalogue| Ok(catalogue.list().map(|s| s.metadata.id.clone()).collect()))?;
        self.pins
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?
            .uploads
            .insert(0, (existing, digests));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_include_prefixed_bare_and_key_digests_only() {
        let a = ContentDigest::of(b"a");
        let b = ContentDigest::of(b"b");
        let c = ContentDigest::of(b"c");
        let value = serde_json::json!({
            "kernel": format!("sha256:{}", a.hex()),
            "nested": [{"sha256": b.hex()}, "sha256:short", 7, null],
            c.hex(): "named by key",
            "upper": a.hex().to_uppercase(),
        });
        let mut digests = BTreeSet::new();
        digests_in(&value, &mut digests);
        assert_eq!(digests, BTreeSet::from([a, b, c]));
    }
}
