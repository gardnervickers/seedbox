//! Closed policy, boot identity, cached RAM evidence and pending report records.
use super::*;
use seedbox_machine::{
    definition::BackendPolicy,
    machine::{EvidenceDisposition, RamRegion, StateEvidence},
    snapshot::{reserve, CodecError, Reader, Writer},
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
fn invalid(s: &'static str) -> CodecError {
    CodecError::Invalid(s)
}
fn put_opt<W: Write>(w: &mut Writer<'_, W>, s: Option<&str>) -> Result<(), CodecError> {
    w.boolean(s.is_some())?;
    if let Some(s) = s {
        w.string(s)?;
    }
    Ok(())
}
fn get_opt<R: Read>(r: &mut Reader<'_, R>) -> Result<Option<String>, CodecError> {
    if r.boolean()? {
        Ok(Some(r.string()?))
    } else {
        Ok(None)
    }
}
fn disposition<W: Write>(w: &mut Writer<'_, W>, d: EvidenceDisposition) -> Result<(), CodecError> {
    match d {
        EvidenceDisposition::Crashed => w.u8(0),
        EvidenceDisposition::Shutdown => w.u8(1),
        EvidenceDisposition::Exited { code } => {
            w.u8(2)?;
            w.u32(code as u32)
        }
        EvidenceDisposition::Live => Err(invalid("live retired disposition")),
    }
}
fn read_disposition<R: Read>(r: &mut Reader<'_, R>) -> Result<EvidenceDisposition, CodecError> {
    match r.u8()? {
        0 => Ok(EvidenceDisposition::Crashed),
        1 => Ok(EvidenceDisposition::Shutdown),
        2 => Ok(EvidenceDisposition::Exited {
            code: r.u32()? as i32,
        }),
        _ => Err(invalid("retired disposition")),
    }
}
fn put_end<W: Write>(w: &mut Writer<'_, W>, e: &Ended) -> Result<(), CodecError> {
    w.u64(e.boot)?;
    disposition(w, e.disposition)?;
    w.boolean(e.native_booted)
}
fn get_end<R: Read>(r: &mut Reader<'_, R>) -> Result<Ended, CodecError> {
    Ok(Ended {
        boot: r.u64()?,
        disposition: read_disposition(r)?,
        native_booted: r.boolean()?,
    })
}
pub(super) fn write<W: Write>(s: &Snapshot, w: &mut Writer<'_, W>) -> Result<(), CodecError> {
    w.u64(s.logical_boot)?;
    w.boolean(s.active_boot.is_some())?;
    if let Some(b) = s.active_boot {
        w.u64(b)?;
    }
    w.boolean(s.policy.is_some())?;
    if let Some(p) = &s.policy {
        let c = &p.configuration;
        w.string(&p.name)?;
        w.u64(c.seed)?;
        w.u64(c.timer_jitter_ns)?;
        w.u64(c.stop_tolerance_ns)?;
        w.boolean(c.torn_writes)?;
        w.boolean(c.full_state)?;
        w.boolean(c.seeded_pv_speed)?;
        w.boolean(c.boot_end_evidence)?;
        put_opt(w, c.controller.as_deref())?;
    }
    w.boolean(s.last_ended.is_some())?;
    if let Some(e) = &s.last_ended {
        put_end(w, e)?;
    }
    w.boolean(s.last_evidence.is_some())?;
    if let Some(e) = &s.last_evidence {
        w.u64(e.boot)?;
        disposition(w, e.disposition)?;
        w.u64(e.parts.len() as u64)?;
        for (n, h) in &e.parts {
            w.string(n)?;
            w.string(h)?;
        }
        w.u64(e.regions.len() as u64)?;
        for r in &e.regions {
            w.u64(r.base)?;
            w.u64(r.length)?;
        }
        w.bytes(&e.page_hashes)?;
        w.string(&e.digest)?;
    }
    s.native_witness.write(w)?;
    w.u64(s.pending_ends.len() as u64)?;
    for p in &s.pending_ends {
        w.u64(p.at.as_nanos())?;
        put_end(w, &p.ended)?;
        w.u64(p.lost)?;
        put_opt(w, p.disk.as_deref())?;
        put_opt(w, p.retired.as_deref())?;
    }
    Ok(())
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
    let logical_boot = r.u64()?;
    let active_boot = if r.boolean()? { Some(r.u64()?) } else { None };
    let policy = if r.boolean()? {
        let name = r.string()?;
        let c = BackendPolicy {
            seed: r.u64()?,
            timer_jitter_ns: r.u64()?,
            stop_tolerance_ns: r.u64()?,
            torn_writes: r.boolean()?,
            full_state: r.boolean()?,
            seeded_pv_speed: r.boolean()?,
            boot_end_evidence: r.boolean()?,
            controller: get_opt(r)?,
        };
        Some(policy::PolicyState::new(&name, &c).map_err(|_| invalid("backend policy"))?)
    } else {
        None
    };
    let last_ended = if r.boolean()? {
        Some(get_end(r)?)
    } else {
        None
    };
    let last_evidence = if r.boolean()? {
        let boot = r.u64()?;
        let disposition = read_disposition(r)?;
        let count = r.count::<(String, String)>(16)?;
        let mut parts = Vec::new();
        reserve(&mut parts, count)?;
        for _ in 0..count {
            parts.push((r.string()?, r.string()?));
        }
        let count = r.count::<RamRegion>(16)?;
        let mut regions = Vec::new();
        reserve(&mut regions, count)?;
        for _ in 0..count {
            regions.push(RamRegion {
                base: r.u64()?,
                length: r.u64()?,
            });
        }
        Some(StateEvidence {
            boot,
            disposition,
            parts,
            regions,
            page_hashes: r.bytes()?,
            digest: r.string()?,
        })
    } else {
        None
    };
    let native_witness = native_witness::NativeWitness::read(r)?;
    let count = r.count::<EndReport>(28)?;
    let mut pending_ends = std::collections::VecDeque::new();
    pending_ends
        .try_reserve(count)
        .map_err(|_| CodecError::Limit("boot-end reports"))?;
    for _ in 0..count {
        pending_ends.push_back(EndReport {
            at: Timestamp::from_nanos(r.u64()?),
            ended: get_end(r)?,
            lost: r.u64()?,
            disk: get_opt(r)?,
            retired: get_opt(r)?,
        });
    }
    Ok(Aux {
        logical_boot,
        active_boot,
        policy,
        last_ended,
        last_evidence,
        native_witness,
        pending_ends,
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
