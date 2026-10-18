//! Actual stopped native/device/PV-page/physical RAM projection. No snapshot
//! file, migration digest, CPU entry, clock adoption or durability mutation.
use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};
// Compile the pure ARM projection on the host for golden/model tests as well.
// Native ARM adapters are still target-gated and never executed by these tests.
#[cfg(any(target_arch = "aarch64", test))]
#[path = "aarch64/backend/evidence.rs"]
pub(super) mod arm;
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
pub(super) fn part(name: &str, value: &impl Serialize) -> Result<(String, String)> {
    let b = serde_json::to_vec(value).map_err(|e| format!("PV evidence {name}: {e}"))?;
    Ok((name.into(), hex(&Sha256::digest(b))))
}
impl<B: Backend> Machine<B> {
    pub(super) fn configure_policy(&mut self, setup: Option<&protocol::BootPolicy>) -> Result<()> {
        if self.state.policy.is_some() || self.state.ready || self.ended || self.state.now != 0 {
            return Err("PV policy requires untouched boot".into());
        }
        self.state.policy = setup.map(super::super::policy::Runtime::new).transpose()?;
        Ok(())
    }
    pub(super) fn policy_setup(&self) -> Option<&protocol::BootPolicy> {
        self.state.policy.as_ref().map(|p| &p.setup)
    }
    pub(super) fn finish_stop(&mut self) -> Result<()> {
        if self.state.policy.is_some() {
            self.native.finish_stop()?;
        }
        Ok(())
    }
    pub(super) fn stopped_evidence(&mut self, path: &Path) -> Result<protocol::NativeEvidence> {
        self.state
            .policy
            .as_ref()
            .ok_or("PV actual evidence requires a configured stopped boundary")?
            .validate()?;
        let mut parts = self.native.stopped_parts()?;
        parts.push(part("clock",&json!({"now":self.state.now,"deadline":self.state.deadline,"timer":self.state.timer,"ready":self.state.ready,"shutdown":self.state.shutdown,"idle":self.state.idle}))?);
        parts.push(part("devices", &self.state.devices)?);
        parts.push(part("console", &self.state.console)?);
        parts.push(part("network", &self.state.net)?);
        parts.push(part("pending_writes", &self.state.writes)?);
        parts.push(part(
            "block_transport",
            &self.block.as_ref().map(|b| b.transport()),
        )?);
        parts.push(part("timer_policy", &self.state.policy)?);
        let layout = self.ram.layout()?;
        let mut regions: Vec<_> = layout
            .segments
            .iter()
            .map(|r| (r.start, r.end - r.start, false))
            .collect();
        regions.push((0x0a00_0000, PAGE_BYTES as u64, true));
        regions.sort_by_key(|r| r.0);
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("PV evidence page file {}: {e}", path.display()))?;
        let mut ram = Sha256::new();
        let mut region_hash = Sha256::new();
        let mut pages = Sha256::new();
        let mut page_bytes = 0u64;
        let mut previous = 0;
        for &(base, len, shared) in &regions {
            if base < previous || len == 0 || base % 4096 != 0 {
                return Err("PV actual mapped RAM regions overlap or are unaligned".into());
            }
            previous = base.checked_add(len).ok_or("PV RAM extent overflow")?;
            region_hash.update(base.to_le_bytes());
            region_hash.update(len.to_le_bytes());
            let bytes = if shared {
                unsafe { self.page.bytes() }
            } else {
                let offset = layout.offset(base, len as usize)?;
                unsafe { &self.ram.bytes()[offset..offset + len as usize] }
            };
            ram.update(bytes);
            for chunk in bytes.chunks(4096) {
                let digest = Sha256::digest(chunk);
                file.write_all(&digest[..8])
                    .map_err(|e| format!("PV evidence pages write: {e}"))?;
                pages.update(&digest[..8]);
                page_bytes = page_bytes.checked_add(8).ok_or("PV page record overflow")?;
            }
        }
        parts.push(("ram".into(), hex(&ram.finalize())));
        parts.push(("ram_regions".into(), hex(&region_hash.finalize())));
        parts.push(("ram_pages".into(), hex(&pages.finalize())));
        // Temporary PR320 probe: the reproduced oracle mismatch is at GPA
        // 0x4591000. Preserve a bounded surrounding window for byte diagnosis.
        if self
            .state
            .policy
            .as_ref()
            .is_some_and(|policy| policy.setup.name == "oracle")
        {
            const START: u64 = 0x0450_0000;
            const LENGTH: usize = 1024 * 1024;
            if let Ok(offset) = layout.offset(START, LENGTH) {
                let bytes = unsafe { &self.ram.bytes()[offset..offset + LENGTH] };
                eprintln!(
                    "pr320-ram-window {}",
                    json!({"base":START,"bytes":hex(bytes)})
                );
            }
        }
        Ok(protocol::NativeEvidence {
            parts,
            regions: regions
                .into_iter()
                .map(|(base, len, _)| (base, len))
                .collect(),
            page_bytes,
        })
    }
}
