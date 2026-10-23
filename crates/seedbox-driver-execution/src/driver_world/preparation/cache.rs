//! Content-addressed finished boot inputs. OCI resolution stays in application;
//! the key follows the explicit inputs consumed by OciBootFactory::boot.
use super::{BootArchive, OciBoot};
use crate::{
    runtime_assets::Inputs,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub struct PreparedBootCache {
    runtime: BTreeMap<String, ContentDigest>,
    tcg: bool,
    epoch: u32,
    blocks: seedbox_spec::progress::ProgressBudget,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedBoot {
    archive: ContentDigest,
    filesystem: ContentDigest,
}

impl PreparedBootCache {
    /// Fingerprint bytes once per preparation, never host paths or timestamps.
    /// Include selected files as well as the package: Inputs exposes their paths,
    /// and factories consume those files rather than rereading the package.
    pub fn new(
        engine: &ContentDigest,
        runtime: &Inputs,
        epoch: u32,
        blocks: seedbox_spec::progress::ProgressBudget,
        store: &ContentStore,
    ) -> Result<Self> {
        let mut digests = BTreeMap::from([
            ("engine".into(), engine.clone()),
            ("package".into(), store.put_file(&runtime.archive())?),
        ]);
        for name in [
            "kernel",
            "init",
            "command-line",
            if runtime.qemu.is_some() {
                "firmware"
            } else {
                "progress.tar"
            },
        ] {
            digests.insert(
                name.into(),
                store.put_file(&runtime.guest_assets.join(name))?,
            );
        }
        let formatter = runtime.guest_assets.join("mke2fs");
        match std::fs::metadata(&formatter) {
            Ok(_) => {
                digests.insert("mke2fs".into(), store.put_file(&formatter)?);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(qemu) = &runtime.qemu {
            digests.insert("qemu".into(), store.put_file(qemu)?);
        }
        Ok(Self {
            runtime: digests,
            tcg: runtime.qemu.is_some(),
            epoch,
            blocks,
        })
    }

    fn key(&self, request: &OciBoot<'_>) -> Result<ContentDigest> {
        let simulation = request.simulation;
        let service = simulation
            .services
            .get(request.name)
            .ok_or_else(|| Error::Integrity("prepared machine has no network address".into()))?;
        let index = simulation
            .services
            .keys()
            .position(|name| name == request.name)
            .ok_or_else(|| Error::Integrity("prepared machine has no network address".into()))?;
        let number = u8::try_from(index + 1).map_err(|e| Error::Integrity(e.to_string()))?;
        let configs: BTreeMap<_, _> = request
            .configs
            .iter()
            .map(|(target, bytes)| (target, ContentDigest::of(bytes)))
            .collect();
        Ok(ContentDigest::of(&serde_json::to_vec(
            &serde_json::json!({
                "kind": "prepared-oci-boot-v1",
                "runtime": self.runtime,
                "tcg": self.tcg,
                "filesystem": ContentDigest::of(&request.filesystem),
                "launch": request.launch,
                "configs": configs,
                "name": request.name,
                "hosts": request.hosts,
                "network": {"mac": [2, 0, 0, 0, 0, number], "address": [10, 0, 0, number], "prefix": 24},
                "control_socket": simulation.control.as_ref().is_some_and(|c| c.service == request.name),
                "memory_mb": service.memory_mb.unwrap_or(simulation.machine.memory_mb),
                "epoch": self.epoch,
                "blocks": self.blocks,
            }),
        )?))
    }

    /// Return complete boot/view digests, rendering only on a miss. Callers must
    /// still validate the runtime and size the boot against machine RAM on hits.
    pub fn prepare<'a>(
        &self,
        request: OciBoot<'a>,
        store: &ContentStore,
        render: impl FnOnce(OciBoot<'a>, &ContentStore) -> Result<BootArchive>,
    ) -> Result<(ContentDigest, ContentDigest)> {
        let key = self.key(&request)?;
        if let Some(boot) = store.record(&key).and_then(|record| {
            let boot: RetainedBoot = serde_json::from_slice(&store.read(&record).ok()?).ok()?;
            store.verify(&boot.archive).ok()?;
            store.verify(&boot.filesystem).ok()?;
            Some(boot)
        }) {
            return Ok((boot.archive, boot.filesystem));
        }
        // Missing/malformed records and absent closure objects are misses. A
        // corrupt content object retains the store's ordinary integrity error
        // when rendering tries to retain it; never repair corruption here.
        let boot = render(request, store)?;
        store.verify(&boot.filesystem)?;
        let boot = RetainedBoot {
            archive: store.put(&boot.bytes)?,
            filesystem: boot.filesystem,
        };
        let record = store.put(&serde_json::to_vec(&boot)?)?;
        store.set_record(&key, &record)?;
        Ok((boot.archive, boot.filesystem))
    }
}
