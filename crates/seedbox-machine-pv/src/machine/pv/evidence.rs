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
/// Stopped CPU parts each architecture's worker reports first, in its order:
/// `worker/x86_64/backend/evidence.rs` and `worker/aarch64/backend/evidence.rs`.
#[cfg(any(target_arch = "x86_64", test))]
const X86_64_CPU: &[&str] = &[
    "regs",
    "sregs",
    "vectors",
    "xcrs",
    "msrs",
    "debugregs",
    "events",
    "vcpu",
    "cpuid",
];
#[cfg(any(target_arch = "aarch64", test))]
const AARCH64_CPU: &[&str] = &["cpu", "gic"];
#[cfg(target_arch = "x86_64")]
const CPU: &[&str] = X86_64_CPU;
#[cfg(target_arch = "aarch64")]
const CPU: &[&str] = AARCH64_CPU;
/// Parts the shared worker and this client append on every architecture.
const MACHINE: [&str; 11] = [
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
pub(super) fn validate(e: &StateEvidence, memory: u64) -> Result<(), String> {
    validate_parts(e, memory, CPU)
}
fn validate_parts(e: &StateEvidence, memory: u64, cpu: &[&str]) -> Result<(), String> {
    e.validate().map_err(str::to_owned)?;
    let disk = e.parts.iter().any(|(n, _)| n == "disk").then_some("disk");
    let names = cpu
        .iter()
        .copied()
        .chain(MACHINE)
        .chain(disk)
        .chain(["policy"]);
    if e.parts.iter().map(|(n, _)| n.as_str()).ne(names) {
        return Err("PV evidence part set/order".into());
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
    let seedbox_machine::definition::BackendPolicy {
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

#[cfg(test)]
mod tests {
    use super::*;
    /// Evidence shaped like a stopped worker's on this host's RAM layout.
    fn evidence(cpu: &[&str], disk: bool) -> StateEvidence {
        let regions: Vec<_> = regions(64 << 20)
            .unwrap()
            .into_iter()
            .map(|(base, length)| RamRegion { base, length })
            .collect();
        let pages: u64 = regions.iter().map(|r| r.length.div_ceil(4096)).sum();
        let page_hashes = vec![7; pages as usize * 8];
        let mut layout = Sha256::new();
        for r in &regions {
            layout.update(r.base.to_le_bytes());
            layout.update(r.length.to_le_bytes());
        }
        let layout = hex(&layout.finalize());
        let page_digest = hex(&Sha256::digest(&page_hashes));
        let names = cpu
            .iter()
            .copied()
            .chain(MACHINE)
            .chain(disk.then_some("disk"))
            .chain(["policy"]);
        let parts: Vec<(String, String)> = names
            .map(|n| {
                let v = match n {
                    "ram_regions" => layout.clone(),
                    "ram_pages" => page_digest.clone(),
                    _ => hex(&Sha256::digest(n)),
                };
                (n.to_owned(), v)
            })
            .collect();
        StateEvidence {
            boot: 1,
            disposition: EvidenceDisposition::Live,
            digest: combined_digest(&parts),
            parts,
            regions,
            page_hashes,
        }
    }
    fn check(cpu: &[&str], e: &StateEvidence) -> Result<(), String> {
        validate_parts(e, 64 << 20, cpu)
    }
    #[test]
    fn each_architecture_accepts_its_own_cpu_parts() {
        for cpu in [X86_64_CPU, AARCH64_CPU] {
            for disk in [false, true] {
                assert_eq!(check(cpu, &evidence(cpu, disk)), Ok(()));
            }
        }
        assert_eq!(
            validate(&evidence(CPU, false), 64 << 20),
            Ok(()),
            "the host architecture validates its own worker parts"
        );
    }
    #[test]
    fn neither_architecture_accepts_the_other_cpu_parts() {
        for (cpu, other) in [(X86_64_CPU, AARCH64_CPU), (AARCH64_CPU, X86_64_CPU)] {
            assert_eq!(
                check(cpu, &evidence(other, false)),
                Err("PV evidence part set/order".into())
            );
        }
    }
    #[test]
    fn missing_or_reordered_parts_are_rejected() {
        for cpu in [X86_64_CPU, AARCH64_CPU] {
            for missing in [cpu[cpu.len() - 1], "clock"] {
                let mut e = evidence(cpu, true);
                e.parts.retain(|(n, _)| n != missing);
                e.digest = combined_digest(&e.parts);
                assert_eq!(check(cpu, &e), Err("PV evidence part set/order".into()));
            }
            let mut e = evidence(cpu, false);
            e.parts.swap(0, 1);
            e.digest = combined_digest(&e.parts);
            assert_eq!(check(cpu, &e), Err("PV evidence part set/order".into()));
        }
    }
}
