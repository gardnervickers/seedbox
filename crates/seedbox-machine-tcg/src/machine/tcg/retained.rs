//! Closed policy, boot identity, cached RAM evidence and pending report records.
use super::*;
use seedbox_machine::{
    machine::{EvidenceDisposition, StateEvidence},
    snapshot::{CodecError, Reader, Record, Writer},
};
use std::io::{Read, Write};
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Ended {
    pub boot: u64,
    pub disposition: EvidenceDisposition,
    pub native_booted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EndReport {
    pub at: Timestamp,
    pub ended: Ended,
    pub lost: u64,
    pub disk: Option<String>,
    pub retired: Option<String>,
}
impl EndReport {
    pub fn response(&self) -> crate::pv::Response {
        crate::pv::Response::BootEnded {
            ended_boot: self.ended.boot,
            disposition: self.ended.disposition,
            native_booted: self.ended.native_booted,
            disk_lost_sectors: self.lost,
            surviving_disk: self.disk.clone(),
            retired_state_digest: self.retired.clone(),
        }
    }
}
seedbox_machine::record!(Ended {
    boot: u64,
    disposition: EvidenceDisposition,
    native_booted: bool,
});
seedbox_machine::record!(EndReport {
    at: Timestamp,
    ended: Ended,
    lost: u64,
    disk: Option<String>,
    retired: Option<String>,
});
fn invalid(s: &'static str) -> CodecError {
    CodecError::Invalid(s)
}
pub(super) fn write<W: Write>(s: &Snapshot, w: &mut Writer<'_, W>) -> Result<(), CodecError> {
    s.logical_boot.encode(w)?;
    s.active_boot.encode(w)?;
    s.policy.encode(w)?;
    s.last_ended.encode(w)?;
    s.last_evidence.encode(w)?;
    s.native_witness.write(w)?;
    s.pending_ends.encode(w)
}
pub(super) struct Aux {
    pub logical_boot: u64,
    pub active_boot: Option<u64>,
    pub policy: Option<policy::PolicyState>,
    pub last_ended: Option<Ended>,
    pub last_evidence: Option<StateEvidence>,
    pub native_witness: native_witness::NativeWitness,
    pub pending_ends: std::collections::VecDeque<EndReport>,
}
pub(super) fn read<R: Read>(r: &mut Reader<'_, R>) -> Result<Aux, CodecError> {
    Ok(Aux {
        logical_boot: Record::decode(r)?,
        active_boot: Record::decode(r)?,
        policy: Record::decode(r)?,
        last_ended: Record::decode(r)?,
        last_evidence: Record::decode(r)?,
        native_witness: native_witness::NativeWitness::read(r)?,
        pending_ends: Record::decode(r)?,
    })
}
pub(super) fn validate(s: &Snapshot) -> Result<(), CodecError> {
    s.native_witness
        .validate_snapshot(s)
        .map_err(|_| invalid("native witness"))?;
    if s.active_boot.is_some() != s.image.is_some()
        || s.active_boot
            .is_some_and(|b| b > s.logical_boot || (b != s.logical_boot && s.pending_boots == 0))
    {
        return Err(invalid("active/logical boot ownership"));
    }
    if s.pending_boots == 1 && (s.logical_boot != 0 || s.last_ended.is_some()) {
        return Err(invalid("initial boot reservation"));
    }
    if let Some(p) = &s.policy {
        p.configuration
            .validate()
            .map_err(|_| invalid("backend policy"))?;
        let expected = format!(
            "seedbox.control={}",
            u8::from(p.configuration.controller.as_deref() == Some(p.name.as_str()))
        );
        match &s.manifest.boot {
            Boot::Linux { command_line, .. } => {
                let mut flags = command_line
                    .split_whitespace()
                    .filter(|s| s.starts_with("seedbox.control="));
                if flags.next() != Some(expected.as_str()) || flags.next().is_some() {
                    return Err(invalid("controller selector differs from named policy"));
                }
            }
            #[cfg(any(test, feature = "test-support"))]
            Boot::Program(_) => return Err(invalid("unsupported persisted policy boot")),
        }
    }
    let full = s
        .policy
        .as_ref()
        .is_some_and(|p| p.configuration.full_state);
    if let Some(e) = &s.last_ended {
        if e.boot > s.logical_boot
            || (!e.native_booted && e.disposition != EvidenceDisposition::Crashed)
        {
            return Err(invalid("ended boot identity"));
        }
    }
    if s.last_evidence.is_some() != (full && s.last_ended.as_ref().is_some_and(|e| e.native_booted))
    {
        return Err(invalid("retired evidence ownership"));
    }
    if let Some(e) = &s.last_evidence {
        let end = s
            .last_ended
            .as_ref()
            .ok_or(invalid("missing retired identity"))?;
        if e.boot != end.boot || e.disposition != end.disposition {
            return Err(invalid("retired evidence identity"));
        }
        evidence::validate(
            e,
            s.manifest.disk.is_some(),
            u64::from(s.manifest.memory_mib) * 1024 * 1024,
        )
        .map_err(|_| invalid("retired state evidence"))?;
    }
    if s.policy.is_some() {
        if s.logical_boot > 0 && s.last_ended.is_none() {
            return Err(invalid("missing prior boot retirement"));
        }
        match s.state {
            State::Exited(reason) => {
                let expected = match reason {
                    ExitReason::Crashed => EvidenceDisposition::Crashed,
                    ExitReason::Shutdown => EvidenceDisposition::Shutdown,
                    ExitReason::ProgramExit { code } => EvidenceDisposition::Exited { code },
                };
                if s.active_boot.is_some()
                    || !s
                        .last_ended
                        .as_ref()
                        .is_some_and(|e| e.boot == s.logical_boot && e.disposition == expected)
                {
                    return Err(invalid("configured ended slot ownership"));
                }
            }
            _ => {
                if s.last_ended
                    .as_ref()
                    .is_some_and(|e| e.boot >= s.logical_boot)
                {
                    return Err(invalid("configured live slot is already retired"));
                }
            }
        }
    }
    let reports = s
        .policy
        .as_ref()
        .is_some_and(|p| p.configuration.boot_end_evidence);
    if !reports && !s.pending_ends.is_empty() {
        return Err(invalid("unconfigured boot-end reports"));
    }
    if let Some(report) = s.pending_ends.back() {
        if Some(&report.ended) != s.last_ended.as_ref() {
            return Err(invalid("pending final retirement identity"));
        }
    }
    let mut previous = None;
    for r in &s.pending_ends {
        if r.ended.boot > s.logical_boot
            || previous.is_some_and(|b| b >= r.ended.boot)
            || r.at > s.reached
            || r.disk.is_some() != s.manifest.disk.is_some()
            || r.disk.is_none() && r.lost != 0
            || r.retired.is_some() != (full && r.ended.native_booted)
        {
            return Err(invalid("pending boot-end ownership"));
        }
        if (!r.ended.native_booted && r.ended.disposition != EvidenceDisposition::Crashed)
            || r.disk
                .iter()
                .chain(&r.retired)
                .any(|v| !evidence::valid_hash(v))
        {
            return Err(invalid("pending boot-end shape"));
        }
        previous = Some(r.ended.boot);
    }
    Ok(())
}
