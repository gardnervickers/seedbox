//! Lightweight native chain. No state-evidence/RAM capture is used here.
//!
//! x86-specific native rows, in addition to the common family table:
//! - J: size=0, a=1 SMI entry / 2 INIT delivery / 3 effective SIPI startup;
//!   b=the 8-bit startup vector only for SIPI, otherwise zero.
//! - G: size=0, a=1 accepted CPU shutdown/triple fault / 2 accepted ACPI GED
//!   reset write, b=0. One per physical boot, latched across migration. This
//!   x86 source mapping is not PV's G row (whose unused a remains zero).
//!
//! Intercepted INIT, SMI still masked/pending, ignored SIPI and APIC poll/TPR
//! are not delivered native records. INIT stamps before reset clears icount
//! bookkeeping, then appends after the accepted effect. No host reset, QMP
//! pause, restore, retirement or successor synthesis appends J/G.
//!
//! The production microvm has ACPI GED but no port92 or i8042 reset device.
//! Seedbox's lifecycle port only retains management bytes/replies; it has no
//! native reset request path to label. A guest-triggered Linux reboot reaches
//! the actual GED reset register or CPU shutdown path, not a fabricated port
//! record. No raw QEMU enum ordinals or interrupt masks enter these rows.
//!
//! MMIO rows represent QEMU's deterministic aligned CPU dispatch pieces,
//! including splits of unaligned/wide accesses. io_prepare's IO recompile
//! longjmp precedes all such rows; both page translations/watchpoints are
//! resolved before dispatch. Transaction errors after dispatch retain actual
//! attempted-piece prefixes, not a successful logical-instruction claim.
//! String PIO records each actual helper invocation/element, not one REP row.
use super::*;
use seedbox_machine::machine::{execution_evidence::chain, ExecutionEvidence};
use seedbox_machine::snapshot::{CodecError, Reader, Writer};
use std::io::{Read, Write};

#[derive(Clone, Default)]
pub(super) struct NativeWitness {
    // Cached pre-loss native observation; disk is always recomputed separately.
    last: Option<Cached>,
    // Accepted predecessor and retirement cause, consumed only by physical boot.
    predecessor: Option<(String, u8)>,
}
#[derive(Clone)]
struct Cached {
    boot: u64,
    disposition: u8,
    code: i32,
    witness: String,
}
fn bytes(s: &str) -> Result<[u8; 32], Error> {
    if !seedbox_machine::machine::is_sha256_hex(s) {
        return Err(Error("TCG native witness: invalid chain SHA256".into()));
    }
    let mut out = [0; 32];
    for (i, v) in out.iter_mut().enumerate() {
        *v = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| Error("TCG native witness: invalid chain byte".into()))?;
    }
    Ok(out)
}
impl NativeWitness {
    pub(super) fn write<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<(), CodecError> {
        w.boolean(self.last.is_some())?;
        if let Some(c) = &self.last {
            w.u64(c.boot)?;
            w.u8(c.disposition)?;
            w.u32(c.code as u32)?;
            w.string(&c.witness)?;
        }
        w.boolean(self.predecessor.is_some())?;
        if let Some((old, mark)) = &self.predecessor {
            w.string(old)?;
            w.u8(*mark)?;
        }
        Ok(())
    }
    pub(super) fn read<R: Read>(r: &mut Reader<'_, R>) -> Result<Self, CodecError> {
        let last = if r.boolean()? {
            Some(Cached {
                boot: r.u64()?,
                disposition: r.u8()?,
                code: r.u32()? as i32,
                witness: r.string()?,
            })
        } else {
            None
        };
        let predecessor = if r.boolean()? {
            Some((r.string()?, r.u8()?))
        } else {
            None
        };
        let value = Self { last, predecessor };
        value
            .validate()
            .map_err(|_| CodecError::Invalid("native witness"))?;
        Ok(value)
    }

    pub(super) fn validate_snapshot(&self, s: &Snapshot) -> Result<(), Error> {
        self.validate()?;
        if s.policy.is_none() && (self.last.is_some() || self.predecessor.is_some()) {
            return Err(Error(
                "TCG native witness has no selected policy owner".into(),
            ));
        }
        if let Some(c) = &self.last {
            if c.boot > s.logical_boot || s.active_boot.is_some_and(|b| b <= c.boot) {
                return Err(Error("TCG cached native witness physical identity".into()));
            }
            if let Some(e) = &s.last_ended {
                let expected = match e.disposition {
                    EvidenceDisposition::Crashed => (1, 0),
                    EvidenceDisposition::Shutdown => (2, 0),
                    EvidenceDisposition::Exited { code } => (3, code),
                    EvidenceDisposition::Live => (0, 0),
                };
                if c.boot == e.boot && (!e.native_booted || (c.disposition, c.code) != expected) {
                    return Err(Error("TCG cached native retirement disposition".into()));
                }
            }
        }
        if s.policy.is_some()
            && s.last_ended.as_ref().is_some_and(|e| e.native_booted)
            && self.last.is_none()
        {
            return Err(Error("TCG missing actual ended native witness".into()));
        }
        if let Some((old, mark)) = &self.predecessor {
            if s.active_boot.is_some() {
                if *mark != chain::REBOOT || s.pending_boots == 0 {
                    return Err(Error("TCG live predecessor lacks accepted reboot".into()));
                }
            } else if self.last.as_ref().is_none_or(|c| {
                c.witness != *old || (*mark == chain::NATURAL_REBOOT) != (c.disposition != 1)
            }) {
                return Err(Error(
                    "TCG predecessor differs from cached retirement".into(),
                ));
            }
        }
        Ok(())
    }
    pub(super) fn validate(&self) -> Result<(), Error> {
        if let Some(c) = &self.last {
            bytes(&c.witness)?;
            if !(1..=3).contains(&c.disposition) || (c.disposition != 3 && c.code != 0) {
                return Err(Error(
                    "TCG native witness: invalid ended disposition".into(),
                ));
            }
        }
        if let Some((old, mark)) = &self.predecessor {
            bytes(old)?;
            if !(1..=3).contains(mark) {
                return Err(Error(
                    "TCG native witness: invalid predecessor cause".into(),
                ));
            }
        }
        Ok(())
    }
}
impl TcgMachine {
    pub(super) fn native_observation(&mut self) -> Result<String, Error> {
        let network = self.manifest.network.is_some();
        let (h, lifecycle, frames) = self.process()?.native_witness(network)?;
        let h = bytes(&h)?;
        // Versioned tuple, no map iteration/host paths/config/clock/RNG. Native
        // queues are canonical strings copied without consuming any device work.
        let pending = serde_json::to_vec(&(
            "tcg-pending-v1",
            lifecycle,
            frames,
            &self.input,
            &self.writes,
            &self.guest,
            self.shutdown_requested,
        ))?;
        Ok(seedbox_machine::machine::hex(&chain::observe(&h, &pending)))
    }
    pub(super) fn cache_native(
        &mut self,
        boot: u64,
        disposition: EvidenceDisposition,
    ) -> Result<(), Error> {
        if self.process.is_none() || self.policy.is_none() {
            return Ok(());
        }
        let witness = match &self.native_witness.predecessor {
            Some((old, chain::REBOOT)) => old.clone(),
            _ => self.native_observation()?,
        };
        let (disposition, code) = match disposition {
            EvidenceDisposition::Crashed => (1, 0),
            EvidenceDisposition::Shutdown => (2, 0),
            EvidenceDisposition::Exited { code } => (3, code),
            EvidenceDisposition::Live => {
                return Err(Error("TCG cannot cache a live retirement".into()))
            }
        };
        // A reboot request may already have captured the live predecessor before
        // clearing frontend queues. Never replace that accepted observation.
        if self.native_witness.predecessor.is_none() {
            self.native_witness.predecessor = Some((
                witness.clone(),
                if disposition == 1 {
                    chain::CRASH
                } else {
                    chain::NATURAL_REBOOT
                },
            ));
        }
        self.native_witness.last = Some(Cached {
            boot,
            disposition,
            code,
            witness,
        });
        Ok(())
    }
    pub(super) fn accept_native_reboot(&mut self) -> Result<(), Error> {
        if self.policy.is_none() {
            return Ok(());
        }
        if self.process.is_some() && self.active_boot.is_some() {
            let old = self.native_observation()?;
            self.native_witness.predecessor = Some((old, chain::REBOOT));
        }
        Ok(())
    }
    pub(super) fn start_native(
        &mut self,
        process: &mut Process,
        boot: u64,
        at: Timestamp,
    ) -> Result<(), Error> {
        if self.policy.is_none() {
            return Ok(());
        }
        let (origin, forced) = match &self.native_witness.predecessor {
            Some((old, mark)) => (
                chain::successor(&bytes(old)?, *mark, at.as_nanos())
                    .map_err(|e| Error(e.into()))?,
                *mark != chain::NATURAL_REBOOT,
            ),
            None => (chain::initial(), false),
        };
        process.start_native_witness(&seedbox_machine::machine::hex(&origin), boot, forced)?;
        self.native_witness.predecessor = None;
        Ok(())
    }
    pub(super) fn execution_observation(&mut self) -> Result<Option<ExecutionEvidence>, Error> {
        if self.policy.is_none() {
            return Ok(None);
        }
        let native = if let Some(boot) = self.active_boot {
            Some((boot, EvidenceDisposition::Live, self.native_observation()?))
        } else {
            self.native_witness.last.as_ref().map(|c| {
                (
                    c.boot,
                    match c.disposition {
                        1 => EvidenceDisposition::Crashed,
                        2 => EvidenceDisposition::Shutdown,
                        _ => EvidenceDisposition::Exited { code: c.code },
                    },
                    c.witness.clone(),
                )
            })
        };
        let Some((boot, disposition, witness)) = native else {
            return Ok(None);
        };
        let store = self.stopped_store()?;
        let torn = self
            .policy
            .as_ref()
            .is_some_and(|p| p.configuration.torn_writes);
        let disk = store
            .as_ref()
            .map(|s| policy::disk_digest(s, torn))
            .transpose()?;
        Ok(Some(ExecutionEvidence {
            boot,
            disposition,
            witness,
            disk,
        }))
    }
}
