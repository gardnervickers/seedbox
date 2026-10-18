//! Actual stopped native parts plus machine-owned pending work and disk state.
use super::*;
use seedbox_machine::machine::{EvidenceDisposition, StateEvidence};
use serde_json::json;
use sha2::{Digest, Sha256};
pub(super) fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn combined(parts: &[(String, String)]) -> String {
    let mut h = Sha256::new();
    for (n, v) in parts {
        h.update(n.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
    }
    policy::hex(&h.finalize())
}
pub(super) fn validate(e: &StateEvidence, disk: bool, memory: u64) -> Result<(), Error> {
    e.validate().map_err(|s| Error(s.into()))?;
    let mut expected = vec![
        "regs",
        "vectors",
        "events",
        "msrs",
        "timers",
        "timer_policy",
        "ram",
        "ram_regions",
        "ram_pages",
        "pending_io",
    ];
    if disk {
        expected.push("disk");
    }
    expected.push("policy");
    if e.parts.iter().map(|(n, _)| n.as_str()).ne(expected) || combined(&e.parts) != e.digest {
        return Err(Error(
            "TCG state-evidence part set or combined digest".into(),
        ));
    }
    if e.parts
        .iter()
        .find(|(n, _)| n == "ram_pages")
        .map(|(_, v)| v.as_str())
        != Some(policy::hex(&Sha256::digest(&e.page_hashes)).as_str())
    {
        return Err(Error("TCG RAM page-record digest".into()));
    }
    let mut total = 0u64;
    let mut region_hash = Sha256::new();
    for r in &e.regions {
        total = total
            .checked_add(r.length)
            .ok_or_else(|| Error("TCG RAM extent overflow".into()))?;
        region_hash.update(r.base.to_le_bytes());
        region_hash.update(r.length.to_le_bytes());
    }
    if total > memory
        || e.parts
            .iter()
            .find(|(n, _)| n == "ram_regions")
            .map(|(_, v)| v.as_str())
            != Some(policy::hex(&region_hash.finalize()).as_str())
    {
        return Err(Error("TCG physical RAM extent digest/capacity".into()));
    }
    Ok(())
}
impl TcgMachine {
    pub(super) fn capture_evidence(
        &mut self,
        boot: u64,
        disposition: EvidenceDisposition,
        store: Option<&Store>,
    ) -> Result<StateEvidence, Error> {
        let capacity = u64::from(self.manifest.memory_mib) * 1024 * 1024;
        let native = self.process()?.state_evidence(capacity)?;
        let mut parts = native.parts;
        parts.push((
            "ram_pages".into(),
            policy::hex(&Sha256::digest(&native.page_hashes)),
        ));
        let ends:Vec<_>=self.pending_ends.iter().map(|r|json!({"at":r.at.as_nanos(),"boot":r.ended.boot,"native":r.ended.native_booted,"lost":r.lost,"disk":r.disk,"retired":r.retired,
            "disposition":match r.ended.disposition{EvidenceDisposition::Live=>0,EvidenceDisposition::Crashed=>1,EvidenceDisposition::Shutdown=>2,EvidenceDisposition::Exited{..}=>3},"exit_code":match r.ended.disposition{EvidenceDisposition::Exited{code}=>Some(code),_=>None}})).collect();
        // Logical pending bytes/identities, not a serialized machine snapshot.
        let pending = serde_json::to_vec(&json!({"input":self.input,"writes":self.writes,
            "bridge":self.guest,"shutdown_requested":self.shutdown_requested,"pending_ends":ends}))
        .map_err(|e| Error(format!("TCG pending-I/O evidence: {e}")))?;
        parts.push(("pending_io".into(), policy::hex(&Sha256::digest(pending))));
        let torn = self
            .policy
            .as_ref()
            .is_some_and(|p| p.configuration.torn_writes);
        if let Some(store) = store {
            parts.push(("disk".into(), policy::disk_digest(store, torn)?));
        }
        let p = self
            .policy
            .as_ref()
            .ok_or_else(|| Error("TCG evidence policy is not configured".into()))?;
        let c = &p.configuration;
        let policy_bytes=serde_json::to_vec(&json!({"name":p.name,"boot":boot,"disposition":match disposition{EvidenceDisposition::Live=>"live",EvidenceDisposition::Crashed=>"crashed",EvidenceDisposition::Shutdown=>"shutdown",EvidenceDisposition::Exited{..}=>"exited"},
            "exit_code":match disposition{EvidenceDisposition::Exited{code}=>Some(code),_=>None},
            "seed":c.seed,"timer_jitter_ns":c.timer_jitter_ns,"stop_tolerance_ns":c.stop_tolerance_ns,
            "torn_writes":c.torn_writes,"full_state":c.full_state,"seeded_pv_speed":c.seeded_pv_speed,
            "boot_end_evidence":c.boot_end_evidence,"controller":c.controller}))
            .map_err(|e|Error(format!("TCG policy evidence: {e}")))?;
        parts.push(("policy".into(), policy::hex(&Sha256::digest(policy_bytes))));
        let e = StateEvidence {
            boot,
            disposition,
            digest: combined(&parts),
            parts,
            regions: native.regions,
            page_hashes: native.page_hashes,
        };
        validate(&e, self.manifest.disk.is_some(), capacity)?;
        Ok(e)
    }
    pub(super) fn stopped_store(&mut self) -> Result<Option<Store>, Error> {
        let Some(disk) = &self.disk else {
            return Ok(None);
        };
        let mut store = match &disk.store {
            Some(s) => return Ok(Some(s.clone())),
            None => disk.empty.clone(),
        };
        let overlays = self.process()?.export_disk()?;
        store
            .replace_overlays(&overlays)
            .map_err(|e| Error(format!("TCG evidence disk export: {e:?}")))?;
        Ok(Some(store))
    }
}
