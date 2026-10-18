//! Parent-process runtime ownership for one concrete executor. No discovery or
//! cache policy lives here: callers supply the Inputs for this retained archive.
#[cfg(test)]
mod tests;
use crate::{runtime_assets::Inputs, store::ContentDigest, Error, Result};
use seedbox_engine_protocol::driver_world::{Artifact, RuntimeBinding};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(crate) struct Binding {
    // Keeps the complete adjacent closure alive through execute()/worker wait.
    _inputs: Inputs,
    pub(crate) files: BTreeMap<ContentDigest, PathBuf>,
    pub(crate) wire: RuntimeBinding,
}
impl Binding {
    pub(crate) fn retained_inputs(&self) -> Inputs {
        self._inputs.clone()
    }
    pub(crate) fn new(inputs: Inputs) -> Result<Self> {
        let archive = artifact(&inputs.archive())?;
        let mut paths = vec![inputs.archive()];
        for name in ["kernel", "init", "command-line", "mke2fs"] {
            paths.push(inputs.guest_assets.join(name));
        }
        if let Some(qemu) = &inputs.qemu {
            paths.push(qemu.clone());
            paths.push(inputs.guest_assets.join("firmware"));
        } else {
            paths.push(inputs.guest_assets.join("progress.tar"));
        }
        let mut files = BTreeMap::new();
        for path in paths {
            files.insert(crate::admission::file_digest(&path)?, path);
        }
        Ok(Self {
            wire: RuntimeBinding {
                archive,
                guest_assets: inputs.guest_assets.clone(),
                qemu: inputs.qemu.clone(),
            },
            files,
            _inputs: inputs,
        })
    }
}
fn artifact(path: &Path) -> Result<Artifact> {
    Ok(Artifact {
        sha256: crate::admission::file_digest(path)?.hex().into(),
        length: fs::metadata(path)?.len(),
        artifact: path
            .to_str()
            .ok_or_else(|| Error::Integrity("Driver materialized path is not UTF-8".into()))?
            .into(),
    })
}
