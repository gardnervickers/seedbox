//! Normal native interaction witness: no RAM or full-state capture dependency.
use super::*;
use seedbox_machine::machine::{execution_evidence::chain, ExecutionEvidence};
impl PVMachine {
    pub(super) fn witness_setup(&self, boot: u64) -> Result<protocol::BootWitness, String> {
        setup(&self.aux, boot)
    }
    /// Capture before clearing client pending IO or destroying the physical
    /// worker. Pending reports/diagnostic capture state are deliberately absent.
    pub(super) fn capture_execution(&mut self) -> Result<[u8; 32], String> {
        let response = self.worker()?.call(Request::ExecutionEvidence)?;
        let Response::ExecutionEvidence(native) = response else {
            return Err("PV expected actual native execution witness".into());
        };
        let client = serde_json::to_vec(&(&self.input, &self.writes, &self.guest, self.shutdown))
            .map_err(|e| format!("PV canonical pending IO: {e}"))?;
        let mut pending = b"pv-client-pending-io-v1\0".to_vec();
        pending.extend_from_slice(&(native.pending.len() as u64).to_le_bytes());
        pending.extend_from_slice(&native.pending);
        pending.extend_from_slice(&(client.len() as u64).to_le_bytes());
        pending.extend_from_slice(&client);
        Ok(chain::observe(&native.chain, &pending))
    }
    pub(super) fn read_execution(&mut self) -> Result<Option<ExecutionEvidence>, String> {
        if self.state == State::Invalid {
            return Err("PV execution evidence requires valid stopped machine".into());
        }
        let (boot, disposition, witness) = if self.worker.is_some() {
            let owner = self
                .aux
                .active
                .ok_or("PV live witness without physical owner")?;
            (
                owner,
                seedbox_machine::machine::EvidenceDisposition::Live,
                self.capture_execution()?,
            )
        } else if let Some(old) = &self.aux.execution {
            (
                old.boot,
                if old.shutdown {
                    seedbox_machine::machine::EvidenceDisposition::Shutdown
                } else {
                    seedbox_machine::machine::EvidenceDisposition::Crashed
                },
                old.witness,
            )
        } else {
            return Ok(None);
        };
        // Current surviving bytes are observed separately: never use pre-loss
        // disk evidence from the native endpoint cache.
        let disk = self
            .stopped_store()?
            .as_ref()
            .map(|store| {
                evidence::disk_digest(
                    store,
                    self.aux
                        .policy
                        .as_ref()
                        .is_some_and(|p| p.settings.torn_writes),
                )
            })
            .transpose()?;
        let result = ExecutionEvidence {
            boot,
            disposition,
            witness: evidence::hex(&witness),
            disk,
        };
        result.validate().map_err(str::to_owned)?;
        Ok(Some(result))
    }
}

pub(super) fn setup(aux: &retained::Aux, boot: u64) -> Result<protocol::BootWitness, String> {
    let initial = match &aux.execution {
        Some(old) => chain::successor(&old.witness, old.mark, old.at).map_err(str::to_owned)?,
        None => chain::initial(),
    };
    Ok(protocol::BootWitness {
        boot,
        initial,
        mark: aux.execution.as_ref().map(|e| e.mark),
    })
}
