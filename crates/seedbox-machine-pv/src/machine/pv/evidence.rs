//! Actual stopped worker evidence plus client-owned pending work and disk.
use super::*;
use seedbox_machine::machine::{
    combined_digest, hex, EvidenceDisposition, RamRegion, StateEvidence,
};
use serde_json::json;
use sha2::{Digest, Sha256};
fn regions(memory: u64) -> Result<Vec<(u64, u64)>, String> {
    let layout = boot::RamLayout::native(memory)?;
    let mut list: Vec<_> = layout
        .segments
        .iter()
        .map(|r| (r.start, r.end - r.start))
        .collect();
    list.push((0x0a00_0000, 16384));
    list.sort_by_key(|r| r.0);
    Ok(list)
}
pub(super) fn validate(e: &StateEvidence, memory: u64) -> Result<(), String> {
    e.validate().map_err(str::to_owned)?;
    #[cfg(target_arch = "x86_64")]
    {
        let mut names = vec![
            "regs",
            "sregs",
            "vectors",
            "xcrs",
            "msrs",
            "debugregs",
            "events",
            "vcpu",
            "cpuid",
            "clock",
            "devices",
            "console",
            "network",
            "pending_writes",
            "block_transport",
            "timer_policy",
            "ram",
            "ram_regions",
            "ram_pages",
            "pending_io",
        ];
        if e.parts.iter().any(|(n, _)| n == "disk") {
            names.push("disk");
        }
        names.push("policy");
        if e.parts.iter().map(|(n, _)| n.as_str()).ne(names) {
            return Err("PV evidence part set/order".into());
        }
    }
    let expected = regions(memory)?;
    if e.regions.iter().map(|r| (r.base, r.length)).ne(expected) {
        return Err("PV evidence mapped RAM differs from actual layout".into());
    }
    if combined_digest(&e.parts) != e.digest {
        return Err("PV evidence combined digest".into());
    }
    let mut layout = Sha256::new();
    for r in &e.regions {
        layout.update(r.base.to_le_bytes());
        layout.update(r.length.to_le_bytes());
    }
    for (name, digest) in [
        ("ram_regions", hex(&layout.finalize())),
        ("ram_pages", hex(&Sha256::digest(&e.page_hashes))),
    ] {
        if e.parts.iter().find(|(n, _)| n == name).map(|(_, v)| v) != Some(&digest) {
            return Err(format!("PV evidence {name} digest"));
        }
    }
    for name in [
        "regs",
        "vectors",
        "clock",
        "devices",
        "console",
        "network",
        "pending_writes",
        "block_transport",
        "timer_policy",
        "ram",
        "pending_io",
        "policy",
    ] {
        if !e.parts.iter().any(|(n, _)| n == name) {
            return Err(format!("PV evidence missing {name}"));
        }
    }
    Ok(())
}
pub(super) fn disk_digest(store: &Store, torn: bool) -> Result<String, String> {
    store
        .state_digest(torn)
        .map_err(|e| format!("PV disk evidence: {e:?}"))
}
pub(super) fn lose(
    store: &Store,
    p: Option<&policy::Named>,
    boot: u64,
) -> Result<(Store, u64), String> {
    let torn = p
        .filter(|p| p.settings.torn_writes)
        .map(|p| (p.settings.seed, p.name.as_str()));
    store.crash(torn, boot).map_err(|e| format!("PV {e}"))
}
pub(super) fn policy_digest(
    policy: &policy::Named,
    boot: u64,
    disposition: EvidenceDisposition,
) -> Result<String, String> {
    let kind = match disposition {
        EvidenceDisposition::Live => 2,
        EvidenceDisposition::Shutdown => 1,
        _ => 0,
    };
    let policy::Named {
        name,
        settings,
        speed_permille,
    } = policy;
    let crate::definition::BackendPolicy {
        seed,
        timer_jitter_ns,
        stop_tolerance_ns,
        torn_writes,
        full_state,
        seeded_pv_speed,
        boot_end_evidence,
        controller,
    } = settings;
    let mut h = Sha256::new();
    h.update(b"seedbox-pv-policy-v1");
    h.update((name.len() as u64).to_le_bytes());
    h.update(name.as_bytes());
    for v in [
        *seed,
        *timer_jitter_ns,
        *stop_tolerance_ns,
        *speed_permille,
        boot,
    ] {
        h.update(v.to_le_bytes());
    }
    for v in [
        *torn_writes,
        *full_state,
        *seeded_pv_speed,
        *boot_end_evidence,
    ] {
        h.update([u8::from(v)]);
    }
    h.update([u8::from(controller.is_some())]);
    if let Some(name) = controller {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
    }
    h.update([kind as u8]);
    Ok(hex(&h.finalize()))
}
impl PVMachine {
    pub(super) fn stopped_store(&mut self) -> Result<Option<Store>, String> {
        let mut store = match &self.disk {
            None => return Ok(None),
            Some(d) => match &d.store {
                Some(s) => return Ok(Some(s.clone())),
                None => d.empty.clone(),
            },
        };
        let directory = tempfile::Builder::new()
            .prefix("disk-export-")
            .tempdir_in(self.directory.path())
            .map_err(|e| format!("PV disk evidence scratch: {e}"))?;
        let path = directory.path().join("overlays");
        if !matches!(
            self.worker()?.call(Request::ExportDisk(path.clone()))?,
            Response::Exported
        ) {
            return Err("PV disk evidence acknowledgement".into());
        }
        let bytes = fs::read(path).map_err(|e| format!("PV disk evidence export: {e}"))?;
        store
            .replace_overlays(&bytes)
            .map_err(|e| format!("PV disk evidence overlays: {e:?}"))?;
        Ok(Some(store))
    }
    pub(super) fn capture_evidence(
        &mut self,
        boot: u64,
        disposition: EvidenceDisposition,
        store: Option<&Store>,
    ) -> Result<StateEvidence, String> {
        let directory = tempfile::Builder::new()
            .prefix("evidence-")
            .tempdir_in(self.directory.path())
            .map_err(|e| format!("PV evidence scratch: {e}"))?;
        let path = directory.path().join("pages");
        let Response::Evidence(native) = self.worker()?.call(Request::Evidence {
            pages: path.clone(),
        })?
        else {
            return Err("PV evidence response is not actual stopped metadata".into());
        };
        let expected = regions(self.memory_bytes)?;
        if native.regions != expected {
            return Err("PV worker evidence RAM layout mismatch".into());
        }
        let mut size = 0u64;
        for &(_, len) in &expected {
            size = size
                .checked_add(
                    len.div_ceil(4096)
                        .checked_mul(8)
                        .ok_or("PV page count overflow")?,
                )
                .ok_or("PV page count overflow")?;
        }
        if native.page_bytes != size {
            return Err("PV evidence page-byte count".into());
        }
        let meta =
            fs::symlink_metadata(&path).map_err(|e| format!("PV evidence page file: {e}"))?;
        if !meta.file_type().is_file() || meta.len() != size {
            return Err("PV evidence pages not a regular exact-length record file".into());
        }
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(usize::try_from(size).map_err(|_| "PV pages exceed host size")?)
            .map_err(|e| format!("PV pages allocation: {e}"))?;
        fs::File::open(&path)
            .map_err(|e| format!("PV pages open: {e}"))?
            .take(size + 1)
            .read_to_end(&mut pages)
            .map_err(|e| format!("PV pages read: {e}"))?;
        if pages.len() as u64 != size {
            return Err("PV evidence page file changed during read".into());
        }
        let mut parts = native.parts;
        let pending=serde_json::to_vec(&json!({"input":self.input,"writes":self.writes,"bridge":self.guest,"shutdown":self.shutdown,"pending_ends":self.aux.pending})).map_err(|e|format!("PV pending evidence: {e}"))?;
        parts.push(("pending_io".into(), hex(&Sha256::digest(pending))));
        let policy = self
            .aux
            .policy
            .as_ref()
            .ok_or("PV evidence policy absent")?;
        if let Some(store) = store {
            parts.push((
                "disk".into(),
                disk_digest(store, policy.settings.torn_writes)?,
            ));
        }
        parts.push(("policy".into(), policy_digest(policy, boot, disposition)?));
        let e = StateEvidence {
            boot,
            disposition,
            digest: combined_digest(&parts),
            parts,
            regions: expected
                .into_iter()
                .map(|(base, length)| RamRegion { base, length })
                .collect(),
            page_hashes: pages,
        };
        validate(&e, self.memory_bytes)?;
        Ok(e)
    }
}
