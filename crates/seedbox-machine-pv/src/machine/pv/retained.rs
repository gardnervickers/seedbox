//! Closed client policy, physical owner and ended evidence. Separate from the
//! bounded private JSON header: page records remain quota-checked raw blobs.
use super::codec::record::Record;
use super::*;
use seedbox_machine::machine::{EvidenceDisposition, RamRegion, StateEvidence};
#[derive(Clone, Debug, Default)]
pub(super) struct Aux {
    pub policy: Option<policy::Named>,
    pub logical: u64,
    pub active: Option<u64>,
    pub last: Option<Ended>,
    pub cache: Option<Cached>,
    pub execution: Option<EndedExecution>,
    pub retiring: Option<[u8; 32]>,
    pub pending: std::collections::VecDeque<Report>,
}
/// Last ACTUAL physical endpoint; retained through logical-only retirements.
#[derive(Clone, Debug)]
pub(super) struct EndedExecution {
    pub boot: u64,
    pub shutdown: bool,
    pub witness: [u8; 32],
    pub mark: u8,
    pub at: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Ended {
    pub boot: u64,
    pub shutdown: bool,
    pub native: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Report {
    pub at: u64,
    pub end: Ended,
    pub lost: u64,
    pub disk: Option<String>,
    pub retired: Option<String>,
}
impl Report {
    pub fn response(&self) -> crate::pv::Response {
        crate::pv::Response::BootEnded {
            ended_boot: self.end.boot,
            disposition: if self.end.shutdown {
                EvidenceDisposition::Shutdown
            } else {
                EvidenceDisposition::Crashed
            },
            native_booted: self.end.native,
            disk_lost_sectors: self.lost,
            surviving_disk: self.disk.clone(),
            retired_state_digest: self.retired.clone(),
        }
    }
}
#[derive(Clone, Debug)]
pub(super) struct Cached {
    pub data: StateEvidence,
}
impl Cached {
    pub fn from(data: StateEvidence) -> Self {
        Self { data }
    }
    pub fn evidence(&self) -> StateEvidence {
        self.data.clone()
    }
}
impl Aux {
    pub fn validate(&self, c: &Capture) -> Result<(), String> {
        if self.active.is_some() != c.live
            || self
                .active
                .is_some_and(|b| self.last.as_ref().is_some_and(|e| e.boot >= b))
            || self
                .active
                .is_some_and(|b| b > self.logical || (b != self.logical && c.pending_boots == 0))
        {
            return Err("PV physical/logical boot ownership".into());
        }
        if (c.pending_boots == 1 && self.logical == 0 && self.last.is_some())
            || (c.pending_boots > 0 && self.logical.checked_sub(c.pending_boots - 1).is_none())
        {
            return Err("PV initial boot reservation".into());
        }
        if self.retiring.is_some() && (self.active.is_none() || c.pending_boots == 0) {
            return Err("PV pending native retirement without active reserved boot".into());
        }
        if let Some(e) = &self.execution {
            if e.boot > self.logical
                || e.at > c.reached
                || !matches!(e.mark, 1..=3)
                || self.active.is_some_and(|active| e.boot >= active)
                || self.last.as_ref().is_none_or(|last| {
                    e.boot > last.boot
                        || (e.boot == last.boot && (!last.native || e.shutdown != last.shutdown))
                })
            {
                return Err("PV execution endpoint owner/cause".into());
            }
        }
        if let Some(p) = &self.policy {
            p.validate()?;
            let expected = format!(
                "seedbox.control={}",
                u8::from(p.settings.controller.as_deref() == Some(p.name.as_str()))
            );
            let mut flags = c
                .command_line
                .split_whitespace()
                .filter(|w| w.starts_with("seedbox.control="));
            if flags.next() != Some(expected.as_str()) || flags.next().is_some() {
                return Err("PV controller selector differs from named policy".into());
            }
            if c.pending_boots == 0
                && matches!(c.state, State::Exited | State::Crashed)
                && !self.last.as_ref().is_some_and(|e| {
                    e.boot == self.logical && e.shutdown == (c.state == State::Exited)
                })
            {
                return Err("PV ended identity differs from slot".into());
            }
            if self.logical > 0 && self.last.is_none() {
                return Err("PV missing earlier retirement".into());
            }
            if !matches!(c.state, State::Exited | State::Crashed)
                && self.last.as_ref().is_some_and(|e| e.boot >= self.logical)
            {
                return Err("PV live slot is already retired".into());
            }
        }
        let full = self.policy.as_ref().is_some_and(|p| p.settings.full_state);
        if let Some(e) = &self.last {
            if e.boot > self.logical || (!e.native && e.shutdown) {
                return Err("PV ended boot identity".into());
            }
        }
        if self.cache.is_some() != (full && self.last.as_ref().is_some_and(|e| e.native)) {
            return Err("PV cached evidence ownership".into());
        }
        if let Some(cache) = &self.cache {
            let last = self.last.as_ref().ok_or("PV cache without end")?;
            let disposition = if last.shutdown {
                EvidenceDisposition::Shutdown
            } else {
                EvidenceDisposition::Crashed
            };
            if cache.data.boot != last.boot
                || cache.data.disposition != disposition
                || cache.data.parts.iter().any(|(n, _)| n == "disk") != c.disk.is_some()
            {
                return Err("PV cached evidence identity/disk ownership".into());
            }
            let selected = self.policy.as_ref().ok_or("PV cached policy absent")?;
            let expected = super::evidence::policy_digest(selected, last.boot, disposition)?;
            if cache
                .data
                .parts
                .iter()
                .find(|(n, _)| n == "policy")
                .map(|(_, v)| v)
                != Some(&expected)
            {
                return Err("PV cached policy digest differs from authority".into());
            }
            super::evidence::validate(&cache.data, c.memory_bytes)?;
        }
        if !self
            .policy
            .as_ref()
            .is_some_and(|p| p.settings.boot_end_evidence)
            && !self.pending.is_empty()
        {
            return Err("PV unconfigured end reports".into());
        }
        if self
            .pending
            .back()
            .is_some_and(|r| Some(&r.end) != self.last.as_ref())
        {
            return Err("PV pending final retirement".into());
        }
        let mut previous = None;
        for r in &self.pending {
            if previous.is_some_and(|p| p >= r.end.boot)
                || r.end.boot > self.logical
                || r.at > c.reached
                || r.disk.is_some() != c.disk.is_some()
                || r.disk.is_none() && r.lost != 0
                || r.retired.is_some() != (full && r.end.native)
                || (!r.end.native && r.end.shutdown)
                || r.disk
                    .iter()
                    .chain(&r.retired)
                    .any(|s| !super::evidence::valid_hash(s))
            {
                return Err("PV pending end shape".into());
            }
            previous = Some(r.end.boot);
        }
        Ok(())
    }
}
macro_rules! record {($ty:ty {$($field:ident:$kind:ty),*$(,)?})=>{impl Record for $ty{const MIN:u64=0$(+<$kind as Record>::MIN)*;fn encode<W:std::io::Write>(&self,w:&mut seedbox_machine::snapshot::Writer<'_,W>)->seedbox_machine::snapshot::Result<()>{let Self{$($field),*}=self;$($field.encode(w)?;)*Ok(())}fn decode<R:std::io::Read>(r:&mut seedbox_machine::snapshot::Reader<'_,R>)->seedbox_machine::snapshot::Result<Self>{Ok(Self{$($field:<$kind as Record>::decode(r)?),*})}}}}
record!(Aux{policy:Option<policy::Named>,logical:u64,active:Option<u64>,last:Option<Ended>,cache:Option<Cached>,execution:Option<EndedExecution>,retiring:Option<[u8;32]>,pending:std::collections::VecDeque<Report>});
record!(EndedExecution {
    boot: u64,
    shutdown: bool,
    witness: [u8; 32],
    mark: u8,
    at: u64
});
record!(Ended {
    boot: u64,
    shutdown: bool,
    native: bool
});
record!(Report{at:u64,end:Ended,lost:u64,disk:Option<String>,retired:Option<String>});
impl Record for RamRegion {
    const MIN: u64 = 16;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self { base, length } = self;
        base.encode(w)?;
        length.encode(w)
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            base: Record::decode(r)?,
            length: Record::decode(r)?,
        })
    }
}
impl Record for Cached {
    const MIN: u64 = 41;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self { data } = self;
        let StateEvidence {
            boot,
            disposition,
            parts,
            regions,
            page_hashes,
            digest,
        } = data;
        boot.encode(w)?;
        match disposition {
            EvidenceDisposition::Crashed => false.encode(w)?,
            EvidenceDisposition::Shutdown => true.encode(w)?,
            _ => {
                return Err(seedbox_machine::snapshot::CodecError::Invalid(
                    "PV cached live disposition",
                ))
            }
        };
        parts.encode(w)?;
        regions.encode(w)?;
        page_hashes.encode(w)?;
        digest.encode(w)
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        let boot = Record::decode(r)?;
        let shutdown = bool::decode(r)?;
        Ok(Self {
            data: StateEvidence {
                boot,
                disposition: if shutdown {
                    EvidenceDisposition::Shutdown
                } else {
                    EvidenceDisposition::Crashed
                },
                parts: Record::decode(r)?,
                regions: Record::decode(r)?,
                page_hashes: Record::decode(r)?,
                digest: Record::decode(r)?,
            },
        })
    }
}
