//! Versioned world composition of real components, not native trace production.
//! Encoding, with machines in Driver INSERTION order:
//! ```text
//! domain; policy\0 + 64 ASCII digest;
//! machine\0 + ordinal:u64LE + name_len:u64LE + UTF8 name + boot:u64LE
//! + disposition:u8 (live0/crash1/shutdown2/exit3 + i32LE code)
//! + native\0 + witness64 + state\0 + presence:u8 [+ digest64]
//! + disk\0 + presence:u8 [+ CURRENT surviving disk64];
//! ...; end\0 + count:u64LE
//! ```
//! This new-engine framing is unambiguous and does not claim byte equality
//! with legacy streaming SHA.
use super::*;
use crate::store::ContentDigest;
use seedbox_machine::machine::ExecutionEvidence;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(in crate::driver_world) fn collect<M>(
    driver: &mut Driver<M>,
    names: &BTreeMap<MachineId, String>,
    policy: &ContentDigest,
    full_state: Option<&StateReport>,
) -> Result<ContentDigest>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    if let Some(state) = full_state {
        state.validate()?;
    }
    let ids = driver
        .with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect::<Vec<_>>()))
        .map_err(|error| Error::Execution(format!("native witness barrier: {error}")))?;
    if ids.is_empty()
        || ids.len() != names.len()
        || full_state.is_some_and(|s| s.machines.len() != ids.len())
    {
        return Err(Error::Integrity(
            "native witness machine inventory differs from stopped Driver".into(),
        ));
    }
    let mut frame = Frame::new(policy);
    for id in ids {
        let name = names.get(&id).ok_or_else(|| {
            Error::Integrity("native witness has an unnamed Driver machine".into())
        })?;
        let native=driver.with_barrier(|barrier|barrier.execution_evidence(id))
            .map_err(|error|Error::Execution(format!("native execution witness for {name}: {error}")))?
            .ok_or_else(||Error::Execution(format!("native execution witness for {name}: no physical boot has supplied evidence, or backend witness capture is not implemented; no witness is fabricated")))?;
        native.validate().map_err(|error| {
            Error::Integrity(format!("native execution witness for {name}: {error}"))
        })?;
        eprintln!("pr320-native-witness {name} {}", native.witness);
        let capture = full_state
            .map(|s| {
                s.machines
                    .get(name)
                    .ok_or_else(|| Error::Integrity(format!("full-state witness missing {name}")))
            })
            .transpose()?;
        if capture.is_some_and(|capture| {
            capture.boot != native.boot || capture.disposition != disposition(native.disposition)
        }) {
            return Err(Error::Integrity(format!(
                "native/full-state witness physical owner or disposition differs for {name}"
            )));
        }
        frame.machine(name, &native, capture.map(|capture| &capture.digest))?;
    }
    frame.finish()
}
struct Frame {
    hash: Sha256,
    count: u64,
    names: BTreeSet<String>,
}
impl Frame {
    fn new(policy: &ContentDigest) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"seedbox-driver-world-witness-v1\0");
        hash.update(b"policy\0");
        hash.update(policy.hex().as_bytes());
        Self {
            hash,
            count: 0,
            names: BTreeSet::new(),
        }
    }
    fn machine(
        &mut self,
        name: &str,
        native: &ExecutionEvidence,
        state: Option<&ContentDigest>,
    ) -> Result<()> {
        native
            .validate()
            .map_err(|error| Error::Integrity(error.into()))?;
        if name.is_empty() || self.names.contains(name) {
            return Err(Error::Integrity(
                "native witness has an empty/duplicate machine name".into(),
            ));
        }
        let count = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::Integrity("native witness machine count overflow".into()))?;
        self.names.insert(name.into());
        self.hash.update(b"machine\0");
        self.hash.update(self.count.to_le_bytes());
        self.hash.update((name.len() as u64).to_le_bytes());
        self.hash.update(name.as_bytes());
        self.hash.update(native.boot.to_le_bytes());
        match native.disposition {
            EvidenceDisposition::Live => self.hash.update([0]),
            EvidenceDisposition::Crashed => self.hash.update([1]),
            EvidenceDisposition::Shutdown => self.hash.update([2]),
            EvidenceDisposition::Exited { code } => {
                self.hash.update([3]);
                self.hash.update(code.to_le_bytes());
            }
        }
        self.hash.update(b"native\0");
        self.hash.update(native.witness.as_bytes());
        self.hash.update(b"state\0");
        self.hash.update([u8::from(state.is_some())]);
        if let Some(state) = state {
            self.hash.update(state.hex().as_bytes());
        }
        self.hash.update(b"disk\0");
        self.hash.update([u8::from(native.disk.is_some())]);
        if let Some(disk) = &native.disk {
            self.hash.update(disk.as_bytes());
        }
        self.count = count;
        Ok(())
    }
    fn finish(mut self) -> Result<ContentDigest> {
        if self.count == 0 {
            return Err(Error::Integrity(
                "Driver policy alone is not a native execution witness".into(),
            ));
        }
        self.hash.update(b"end\0");
        self.hash.update(self.count.to_le_bytes());
        Ok(ContentDigest::finish(self.hash))
    }
}
#[cfg(test)]
mod tests;
