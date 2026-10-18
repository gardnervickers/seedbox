//! Offline capture of a checkpoint and its owned binary assets.
use crate::{store::ContentStore, Error, Result};
use seedbox_spec::definition::{Asset, Start};
use std::collections::BTreeMap;
fn asset(d: &crate::store::ContentDigest) -> Asset {
    Asset(format!("sha256:{}", d.hex()))
}
pub fn capture_start(
    path: &std::path::Path,
    owned: &std::path::Path,
    store: &ContentStore,
) -> Result<Start> {
    capture_start_observed(path, owned, store, &mut |_| Ok(()))
}

/// Capture with cooperative byte-work observation, including verification passes.
/// An observer error stops capture without returning a partial starting state.
/// Already retained immutable assets remain reusable in the shared store.
pub fn capture_start_observed(
    path: &std::path::Path,
    owned: &std::path::Path,
    store: &ContentStore,
    progress: &mut impl FnMut(u64) -> std::io::Result<()>,
) -> Result<Start> {
    progress(0)?;
    let mut manifest: serde_json::Value = serde_json::from_reader(std::fs::File::open(path)?)?;
    let owned = owned.canonicalize()?;
    let mut assets = BTreeMap::new();
    fn visit(
        value: &mut serde_json::Value,
        owned: &std::path::Path,
        store: &ContentStore,
        assets: &mut BTreeMap<String, Asset>,
        progress: &mut impl FnMut(u64) -> std::io::Result<()>,
    ) -> Result<()> {
        progress(0)?;
        match value {
            serde_json::Value::Object(values)
                if values.get("sha256").and_then(|v| v.as_str()).is_some()
                    && (values.get("artifact").and_then(|v| v.as_str()).is_some()
                        || values.get("path").and_then(|v| v.as_str()).is_some()) =>
            {
                let key = if values.contains_key("artifact") {
                    "artifact"
                } else {
                    "path"
                };
                let text = values[key].as_str().ok_or_else(|| {
                    Error::Integrity("checkpoint binary path is not a string".into())
                })?;
                let path = std::path::Path::new(text).canonicalize()?;
                if !path.starts_with(owned) {
                    return Err(Error::Integrity(
                        "checkpoint binary refers outside its owned execution directory".into(),
                    ));
                }
                let name = format!("state-{}", assets.len() + 1);
                assets.insert(
                    name.clone(),
                    asset(&store.put_file_observed(&path, progress)?),
                );
                values.insert(key.into(), format!("asset:{name}").into());
                for value in values.values_mut() {
                    visit(value, owned, store, assets, progress)?;
                }
            }
            serde_json::Value::Array(values) => {
                for v in values {
                    visit(v, owned, store, assets, progress)?;
                }
            }
            serde_json::Value::Object(values) => {
                for v in values.values_mut() {
                    visit(v, owned, store, assets, progress)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(&mut manifest, &owned, store, &mut assets, progress)?;
    Ok(Start::Checkpoint { manifest, assets })
}
