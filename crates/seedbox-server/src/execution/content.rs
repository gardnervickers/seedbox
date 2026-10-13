//! Reclaim the cache objects that only deleted resources referenced.
//!
//! Checkpoint state, imported binaries and edited boot archives are created as
//! owned cache objects (`ContentStore::put_*_owned`). After a deletion, every
//! owned object that no remaining resource, retained input file or exported
//! checkpoint of a remaining source names is removed. Reusable cache inputs
//! (kernels, prepared images and guests) are never owned, so this is not cache
//! pruning. Work that creates an owned object holds `references` for reading
//! until a resource or the export registry names it; reclamation holds it for
//! writing, after the evidence lock that orders imports, exports and deletes.
use super::{Error, ExecutionService};
use crate::catalogue;
use seedbox_api::SimulationId;
use seedbox_application::store::ContentDigest;
use std::{collections::BTreeSet, fs};

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

impl ExecutionService {
    /// Remove owned content that no remaining resource references. Deletion
    /// does this itself; the server also calls it before a graceful shutdown,
    /// after which still referenced objects remain as ordinary cache content.
    pub fn reclaim_content(&self) -> Result<Vec<ContentDigest>, Error> {
        let _evidence = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        self.reclaim_unreferenced()
    }

    /// Callers hold the evidence lock.
    pub(super) fn reclaim_unreferenced(&self) -> Result<Vec<ContentDigest>, Error> {
        let _references = self
            .references
            .write()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let (mut roots, ids) = self.manager.read(|catalogue| {
            let ids: Vec<SimulationId> = catalogue
                .list()
                .map(|resource| resource.metadata.id.clone())
                .collect();
            Ok((catalogue.referenced_content()?, ids))
        })?;
        for id in ids {
            match fs::read(self.root.join("inputs").join(format!("{id}.json"))) {
                Ok(bytes) => {
                    let value: serde_json::Value =
                        serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                    digests_in(&value, &mut roots);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let exports = self
            .exports
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        roots.extend(
            exports
                .values()
                .flat_map(|captures| captures.values().flatten().cloned()),
        );
        Ok(self.store.reclaim_owned(&roots)?)
    }

    /// Keep an exported checkpoint's captured state while its source and the
    /// checkpoint remain, so a client can still submit the branch it fetched.
    pub(super) fn record_export(
        &self,
        id: &SimulationId,
        name: &str,
        start: &seedbox_spec::definition::Start,
    ) -> Result<(), Error> {
        let mut digests = BTreeSet::new();
        digests_in(
            &serde_json::to_value(start).map_err(std::io::Error::other)?,
            &mut digests,
        );
        self.exports
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?
            .entry(id.clone())
            .or_default()
            .insert(name.to_owned(), digests);
        Ok(())
    }

    pub(super) fn forget_exports(
        &self,
        id: &SimulationId,
        name: Option<&str>,
    ) -> Result<(), Error> {
        let mut exports = self
            .exports
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        match name {
            None => {
                exports.remove(id);
            }
            Some(name) => {
                if let Some(captures) = exports.get_mut(id) {
                    captures.remove(name);
                }
            }
        }
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
