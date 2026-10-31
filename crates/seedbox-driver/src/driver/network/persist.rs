//! Field and tag order is the checkpoint encoding; changing it needs a
//! checkpoint `VERSION` bump.
use super::*;
wire_struct!(Member {
    name,
    seed,
    mac,
    address,
    sent
});
mod parity_persist;
impl crate::persistence::Wire for Pair {
    fn put<W: std::io::Write>(
        &self,
        e: &mut crate::persistence::Encoder<W>,
    ) -> crate::persistence::Result<()> {
        self.loss.put(e)?;
        self.jitter.put(e)?;
        self.last.put(e)?;
        self.last_depart.put(e)?;
        for rng in &self.faults {
            rng.put(e)?;
        }
        Ok(())
    }
    fn get<R: std::io::Read>(
        d: &mut crate::persistence::Decoder<R>,
    ) -> crate::persistence::Result<Self> {
        Ok(Self {
            loss: crate::persistence::Wire::get(d)?,
            jitter: crate::persistence::Wire::get(d)?,
            last: crate::persistence::Wire::get(d)?,
            last_depart: crate::persistence::Wire::get(d)?,
            faults: [
                crate::persistence::Wire::get(d)?,
                crate::persistence::Wire::get(d)?,
                crate::persistence::Wire::get(d)?,
                crate::persistence::Wire::get(d)?,
                crate::persistence::Wire::get(d)?,
            ],
        })
    }
}

impl crate::persistence::Wire for NetworkConfig {
    fn put<W: std::io::Write>(
        &self,
        e: &mut crate::persistence::Encoder<W>,
    ) -> crate::persistence::Result<()> {
        self.latency_ns.put(e)?;
        self.jitter_ns.put(e)?;
        self.loss_ppm.put(e)
    }
    fn get<R: std::io::Read>(
        d: &mut crate::persistence::Decoder<R>,
    ) -> crate::persistence::Result<Self> {
        Self::new(
            Duration::from_nanos(u64::get(d)?),
            Duration::from_nanos(u64::get(d)?),
            u32::get(d)?,
        )
        .map_err(|_| crate::persistence::CodecError::Invalid("network configuration"))
    }
}
impl Network {
    pub(crate) fn member_matches_persist(
        &self,
        id: usize,
        name: Option<&str>,
        attached: bool,
    ) -> bool {
        self.members
            .get(id)
            .is_some_and(|m| m.name == name.unwrap_or("") && m.mac.is_some() == attached)
    }
    pub(crate) fn member_seed(&self, id: usize) -> u64 {
        self.members[id].seed
    }
    pub(crate) fn check_persist(
        &self,
        n: usize,
        now: u64,
        definitions: &[crate::definition::MachineDefinition],
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        require(self.members.len() == n, "network members")?;
        let mut macs = std::collections::BTreeSet::new();
        let mut addresses = std::collections::BTreeSet::new();
        for m in &self.members {
            require(m.mac.is_some() == m.address.is_some(), "member address")?;
            if let (Some(mac), Some(address)) = (m.mac, m.address) {
                require(macs.insert(mac), "duplicate member MAC")?;
                require(addresses.insert(address), "duplicate member address")?;
                require(!super::reserved(mac, address), "member uses the nameserver")?;
            }
        }
        for d in definitions {
            if let Some(nic) = &d.network {
                require(macs.insert(nic.mac()), "duplicate future MAC")?;
                require(
                    !super::reserved(nic.mac(), nic.ipv4().octets()),
                    "future machine uses the nameserver",
                )?;
            }
        }
        for (&(a, b), p) in &self.pairs {
            require(
                a.0 < n && b.0 < n && a != b && self.attached(a) && self.attached(b),
                "network pair",
            )?;
            let seed = pair_seed(self.members[a.0].seed, &self.members[b.0].name);
            p.loss.check_persist(RngDomain::NetworkLoss, Some(seed))?;
            p.jitter
                .check_persist(RngDomain::NetworkJitter, Some(seed))?;
            for (rng, domain) in p.faults.iter().zip(super::parity::FAULT_DOMAINS) {
                rng.check_persist(domain, Some(seed))?;
            }
            require(
                p.last_depart
                    .is_none_or(|d| p.last.is_some_and(|last| d <= last)),
                "pair departure/FIFO",
            )?;
        }
        for (&(at, a, seq, b), bytes) in &self.in_flight {
            require(
                a.0 < n
                    && b.0 < n
                    && at >= now
                    && bytes.sent <= now
                    && crate::pv::frame_in_bounds(&bytes.bytes),
                "network flight",
            )?;
            require(
                seq < self.members[a.0].sent
                    && (a == b && self.attached(a)
                        || self.pairs.get(&(a, b)).is_some_and(|p| p.last.is_some()))
                    && bytes
                        .sent
                        .checked_add(self.config.latency_ns)
                        .is_some_and(|earliest| at >= earliest),
                "flight sequence/FIFO",
            )?;
        }
        let mut cuts = std::collections::BTreeMap::new();
        for pairs in self.partitions.values() {
            let mut seen = std::collections::BTreeSet::new();
            for &(a, b) in pairs {
                require(
                    a.0 < n && b.0 < n && a != b && seen.insert((a, b)),
                    "partition pair",
                )?;
                let c = cuts.entry((a, b)).or_insert(0u32);
                *c = c
                    .checked_add(1)
                    .ok_or(crate::persistence::CodecError::Invalid("cut overflow"))?;
            }
        }
        require(cuts == self.cuts, "partition cuts")?;
        self.check_parity_persist(n, now, definitions)
    }
}

impl Network {
    pub(crate) fn scratch_persist(&self) -> crate::persistence::Result<u64> {
        let pairs = self.partitions.values().try_fold(0u64, |n, p| {
            n.checked_add(p.len() as u64)
                .ok_or(crate::persistence::CodecError::Limit("scratch overflow"))
        })?;
        (self.members.len() as u64)
            .checked_mul(std::mem::size_of::<MacAddress>() as u64)
            .and_then(|n| {
                pairs
                    .checked_mul(2 * std::mem::size_of::<((MachineId, MachineId), u32)>() as u64)
                    .and_then(|p| n.checked_add(p))
            })
            .ok_or(crate::persistence::CodecError::Limit("scratch overflow"))
    }
}
