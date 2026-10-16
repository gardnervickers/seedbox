//! Version 2 closed network policy/observation and canonical tracker records.
use super::*;
use crate::driver::Position;
use crate::persistence::{require, CodecError, Decoder, Encoder, Result, Wire};
use seedbox_engine_core::connections::{
    ConnectionState, Connections, SelectionState, SessionState,
};
use std::io::{Read, Write};
wire_struct!(NetworkPolicy {
    latency_ns,
    jitter_ns,
    loss_ppm,
    bandwidth_bps,
    queue_bound_ns
});
wire_struct!(NetworkFaults {
    reorder_ppm,
    reorder_delay_ns,
    duplicate_ppm,
    burst_loss_ppm,
    burst_length,
    latency_spike_ppm,
    latency_spike_ns,
    tcp_reset_ppm
});
wire_struct!(QueueCounts {
    matched,
    queue_dropped,
    delayed_max_ns
});
wire_struct!(NetworkFaultCounts {
    matched,
    reordered,
    duplicated,
    burst_dropped,
    spiked,
    tcp_reset,
    delayed_max_ns
});
wire_struct!(NetworkPairCounts {
    delivered,
    loss_dropped,
    partition_dropped,
    queue_dropped,
    receiver_dropped
});
wire_struct!(SelectionCounts { matched, dropped });
wire_struct!(NetworkSettlement {
    queue,
    faults,
    selection
});
impl Wire for Connection {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.from.put(e)?;
        self.to.put(e)?;
        self.src_addr.octets().put(e)?;
        self.dst_addr.octets().put(e)?;
        self.src_port.put(e)?;
        self.dst_port.put(e)?;
        self.incarnation.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Ok(Self {
            from: Wire::get(d)?,
            to: Wire::get(d)?,
            src_addr: <[u8; 4]>::get(d)?.into(),
            dst_addr: <[u8; 4]>::get(d)?.into(),
            src_port: Wire::get(d)?,
            dst_port: Wire::get(d)?,
            incarnation: Wire::get(d)?,
        })
    }
}
wire_struct!(Flight { sent, bytes });
wire_struct!(Layer {
    handle,
    pairs,
    policy,
    faults,
    queue,
    counts,
    burst
});
wire_enum!(FrameFate;units{0=>Scheduled,1=>PartitionDropped,2=>LossDropped,3=>QueueDropped,4=>ReceiverDropped}tuples{}records{});
wire_struct!(FrameFact {
    position,
    from,
    to,
    sent,
    due,
    bytes,
    fate
});
impl Wire for SessionState {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.id.put(e)?;
        self.syn[0].put(e)?;
        self.syn[1].put(e)?;
        self.fin[0].put(e)?;
        self.fin[1].put(e)?;
        self.closed.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Ok(Self {
            id: Wire::get(d)?,
            syn: [Wire::get(d)?, Wire::get(d)?],
            fin: [Wire::get(d)?, Wire::get(d)?],
            closed: Wire::get(d)?,
        })
    }
}
wire_struct!(SelectionState {
    fault_id,
    connection,
    identity,
    matched,
    dropped
});
type SessionKey = ((usize, [u8; 4], u16), (usize, [u8; 4], u16));
impl Wire for ConnectionState {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.sessions.put(e)?;
        self.next.put(e)?;
        self.evicted.put(e)?;
        self.used.put(e)?;
        self.selections.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        let n = u64::get(d)?;
        let count = d.bounded_count::<(SessionKey, SessionState)>(
            n,
            seedbox_engine_core::connections::CAPACITY as u64,
            41,
        )?;
        // The canonical target map is a distinct construction; charge its
        // elements/items before retaining any source DTO element.
        d.claim_scratch::<(SessionKey, SessionState)>(n)?;
        let mut sessions = BTreeMap::new();
        for _ in 0..count {
            let key = SessionKey::get(d)?;
            require(
                sessions
                    .last_key_value()
                    .is_none_or(|(last, _)| last < &key),
                "connection session order",
            )?;
            sessions.insert(key, SessionState::get(d)?);
        }
        let next = Wire::get(d)?;
        let evicted = Wire::get(d)?;
        let used = Wire::get(d)?;
        let n = u64::get(d)?;
        let count = d.bounded_count::<(String, SelectionState)>(
            n,
            seedbox_engine_core::connections::CAPACITY as u64,
            100,
        )?;
        d.claim_scratch::<(String, SelectionState)>(n)?;
        let mut selections = BTreeMap::new();
        for _ in 0..count {
            let key = String::get(d)?;
            require(
                selections
                    .last_key_value()
                    .is_none_or(|(last, _)| last < &key),
                "connection selection order",
            )?;
            selections.insert(key, SelectionState::get(d)?);
        }
        Ok(Self {
            sessions,
            next,
            evicted,
            used,
            selections,
        })
    }
}
impl Wire for Network {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        let Self {
            config,
            members,
            pairs,
            in_flight,
            cuts,
            partitions,
            global,
            baselines,
            layers,
            connections,
            queue_counts,
            counts,
            frame_capacity,
            frames,
            omitted,
        } = self;
        config.put(e)?;
        members.put(e)?;
        pairs.put(e)?;
        in_flight.put(e)?;
        cuts.put(e)?;
        partitions.put(e)?;
        global.put(e)?;
        baselines.put(e)?;
        layers.put(e)?;
        connections.export_state().put(e)?;
        queue_counts.put(e)?;
        counts.put(e)?;
        frame_capacity.put(e)?;
        frames.put(e)?;
        omitted.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        let config = Wire::get(d)?;
        let members: Vec<Member> = Wire::get(d)?;
        let pairs = Wire::get(d)?;
        let in_flight = Wire::get(d)?;
        let cuts = Wire::get(d)?;
        let partitions = Wire::get(d)?;
        let global = Wire::get(d)?;
        let baselines = Wire::get(d)?;
        let layers = Wire::get(d)?;
        let state = ConnectionState::get(d)?;
        // The DTO's maps are charged by their closed decoder. Import constructs
        // canonical maps again while moving (not cloning) their owned strings.
        let names_bytes = members.iter().try_fold(0u64, |n, m| {
            n.checked_add(m.name.len() as u64)
                .ok_or(CodecError::Limit("tracker name storage"))
        })?;
        d.claim_scratch::<String>(members.len() as u64)?;
        d.charge(names_bytes)?;
        let mut names = Vec::new();
        names
            .try_reserve_exact(members.len())
            .map_err(|_| CodecError::Limit("allocation failed"))?;
        for member in &members {
            let mut name = String::new();
            name.try_reserve_exact(member.name.len())
                .map_err(|_| CodecError::Limit("allocation failed"))?;
            name.push_str(&member.name);
            names.push(name);
        }
        let connections = Connections::from_state(state, &names)
            .map_err(|_| CodecError::Invalid("connection tracker"))?;
        let queue_counts = Wire::get(d)?;
        let counts = Wire::get(d)?;
        let frame_capacity: usize = Wire::get(d)?;
        let frames: std::collections::VecDeque<FrameFact> = Wire::get(d)?;
        let omitted = Wire::get(d)?;
        require(
            frame_capacity as u64 <= d.limits.collection_items && frames.len() <= frame_capacity,
            "frame observation capacity",
        )?;
        Ok(Self {
            config,
            members,
            pairs,
            in_flight,
            cuts,
            partitions,
            global,
            baselines,
            layers,
            connections,
            queue_counts,
            counts,
            frame_capacity,
            frames,
            omitted,
        })
    }
}

impl Network {
    pub(super) fn check_parity_persist(
        &self,
        n: usize,
        now: u64,
        definitions: &[crate::definition::MachineDefinition],
    ) -> Result<()> {
        require(
            self.global.latency_ns == self.config.latency_ns
                && self.global.jitter_ns == self.config.jitter_ns
                && self.global.loss_ppm == u64::from(self.config.loss_ppm),
            "global network policy",
        )?;
        self.global
            .validate(self.config.latency_ns)
            .map_err(|_| CodecError::Invalid("global network policy"))?;
        let mut names: std::collections::BTreeSet<&str> = self
            .members
            .iter()
            .filter(|m| m.mac.is_some())
            .map(|m| m.name.as_str())
            .collect();
        names.extend(
            definitions
                .iter()
                .filter(|d| d.network.is_some())
                .map(|d| d.name.as_str()),
        );
        for ((a, b), policy) in &self.baselines {
            require(
                a != b && names.contains(a.as_str()) && names.contains(b.as_str()),
                "baseline pair",
            )?;
            policy
                .validate(self.config.latency_ns)
                .map_err(|_| CodecError::Invalid("baseline policy"))?;
        }
        let pair_valid = |(a, b): (MachineId, MachineId)| {
            a != b && a.0 < n && b.0 < n && self.attached(a) && self.attached(b)
        };
        let mut handles = std::collections::BTreeSet::new();
        for (&id, l) in &self.layers {
            require(
                id > 0
                    && !l.handle.is_empty()
                    && handles.insert(l.handle.as_str())
                    && !l.pairs.is_empty(),
                "shape identity",
            )?;
            l.policy
                .validate_shape(self.config.latency_ns, &l.faults)
                .map_err(|_| CodecError::Invalid("shape policy"))?;
            for &pair in &l.pairs {
                require(pair_valid(pair), "shape pair")?;
            }
            for (&pair, &remaining) in &l.burst {
                require(
                    l.pairs.contains(&pair)
                        && l.faults.burst_loss_ppm != 0
                        && remaining < l.faults.burst_length,
                    "burst continuation",
                )?;
            }
            require(l.queue.queue_dropped <= l.queue.matched, "queue counts")?;
            require(
                l.policy.bandwidth_bps.is_some() || l.queue == QueueCounts::default(),
                "disabled shape queue counts",
            )?;
            require(
                !l.faults.is_off() || l.counts == NetworkFaultCounts::default(),
                "disabled shape fault counts",
            )?;
            for (ppm, count) in [
                (l.faults.reorder_ppm, l.counts.reordered),
                (l.faults.duplicate_ppm, l.counts.duplicated),
                (l.faults.burst_loss_ppm, l.counts.burst_dropped),
                (l.faults.latency_spike_ppm, l.counts.spiked),
                (l.faults.tcp_reset_ppm, l.counts.tcp_reset),
            ] {
                require(ppm != 0 || count == 0, "disabled impairment count")?;
            }
            if !l.faults.is_off() {
                let after = l
                    .counts
                    .matched
                    .checked_sub(l.counts.burst_dropped)
                    .and_then(|n| n.checked_sub(l.queue.queue_dropped))
                    .ok_or(CodecError::Invalid("shape early-drop counts"))?;
                require(
                    [
                        l.counts.reordered,
                        l.counts.duplicated,
                        l.counts.spiked,
                        l.counts.tcp_reset,
                    ]
                    .into_iter()
                    .all(|n| n <= after)
                        && l.queue.matched <= l.counts.matched - l.counts.burst_dropped,
                    "shape post-drop counts",
                )?;
            }
            for c in [
                l.counts.reordered,
                l.counts.duplicated,
                l.counts.burst_dropped,
                l.counts.spiked,
                l.counts.tcp_reset,
            ] {
                require(c <= l.counts.matched, "network fault counts")?;
            }
        }
        for &pair in self.counts.keys().chain(self.queue_counts.keys()) {
            require(pair_valid(pair), "network counter pair")?;
        }
        for counts in self.queue_counts.values() {
            require(counts.queue_dropped <= counts.matched, "pair queue counts")?;
        }
        require(
            self.frames.len() <= self.frame_capacity
                && (self.frame_capacity != 0 || (self.frames.is_empty() && self.omitted == 0)),
            "frame observation capacity",
        )?;
        for f in &self.frames {
            require(
                pair_valid((f.from, f.to))
                    && f.position.at.as_nanos() <= now
                    && f.sent <= f.position.at
                    && f.bytes >= 14
                    && f.bytes <= crate::pv::MAX_FRAME_BYTES,
                "frame fact",
            )?;
            require(
                match f.fate {
                    FrameFate::Scheduled | FrameFate::ReceiverDropped => f
                        .sent
                        .as_nanos()
                        .checked_add(self.config.latency_ns)
                        .is_some_and(|floor| f.due.is_some_and(|due| due.as_nanos() >= floor)),
                    _ => f.due.is_none(),
                },
                "frame fate deadline",
            )?;
        }
        Ok(())
    }
}

impl NetworkSettlement {
    pub(crate) fn check_persist(&self) -> Result<()> {
        require(
            self.selection.is_none() || (self.queue.is_none() && self.faults.is_none()),
            "mixed network settlement",
        )?;
        if let Some(c) = self.selection {
            require(c.dropped <= c.matched, "selection counts")?;
        }
        if let Some(c) = self.queue {
            require(c.queue_dropped <= c.matched, "queue counts")?;
        }
        if let Some(c) = self.faults {
            for count in [
                c.reordered,
                c.duplicated,
                c.burst_dropped,
                c.spiked,
                c.tcp_reset,
            ] {
                require(count <= c.matched, "network fault counts")?;
            }
        }
        Ok(())
    }
}
impl Network {
    pub(crate) fn check_owner_persist(
        &self,
        handle: &str,
        change: crate::oracle::HandleChange,
        target: &crate::oracle::HandleTarget,
        resolve: &impl Fn(&str) -> std::result::Result<MachineId, String>,
    ) -> Result<()> {
        use crate::oracle::{HandleChange as Change, HandleTarget};
        let id =
            |name: &str| resolve(name).map_err(|_| CodecError::Invalid("network handle member"));
        match (change, target) {
            (Change::Partition, HandleTarget::Links { left, right }) => {
                let pairs = self
                    .partitions
                    .get(handle)
                    .ok_or(CodecError::Invalid("partition owner"))?;
                require(
                    left.len()
                        .checked_mul(right.len())
                        .and_then(|n| n.checked_mul(2))
                        == Some(pairs.len()),
                    "partition owner pairs",
                )?;
                let mut cursor = 0;
                for a in left {
                    for b in right {
                        let (a, b) = (id(a)?, id(b)?);
                        require(
                            pairs[cursor] == (a, b) && pairs[cursor + 1] == (b, a),
                            "partition owner pairs",
                        )?;
                        cursor += 2;
                    }
                }
            }
            (Change::Shape(policy, faults), HandleTarget::DirectedLinks { from, to }) => {
                let layer = self
                    .layers
                    .values()
                    .find(|l| l.handle == handle)
                    .ok_or(CodecError::Invalid("shape owner"))?;
                require(
                    layer.policy == policy
                        && layer.faults == faults
                        && from.len().checked_mul(to.len()) == Some(layer.pairs.len()),
                    "shape owner policy",
                )?;
                for a in from {
                    for b in to {
                        require(layer.pairs.contains(&(id(a)?, id(b)?)), "shape owner pairs")?;
                    }
                }
            }
            (Change::Select, HandleTarget::Connection(c)) => require(
                self.connections
                    .selections
                    .get(handle)
                    .is_some_and(|s| s.connection == *c),
                "selection owner",
            )?,
            _ => return Err(CodecError::Invalid("network owner target")),
        }
        Ok(())
    }
    pub(crate) fn check_owner_set_persist(
        &self,
        expected: &std::collections::BTreeSet<String>,
        token: &impl Fn(u32) -> bool,
    ) -> Result<()> {
        let count = self
            .partitions
            .len()
            .checked_add(self.layers.len())
            .and_then(|n| n.checked_add(self.connections.selections.len()))
            .ok_or(CodecError::Invalid("network owner count"))?;
        require(count == expected.len(), "network owner set")?;
        for name in self
            .partitions
            .keys()
            .chain(self.layers.values().map(|l| &l.handle))
            .chain(self.connections.selections.keys())
        {
            require(expected.contains(name), "orphan network handle")?;
        }
        for &id in self.layers.keys() {
            require(token(id), "shape fault token")?;
        }
        for selection in self.connections.selections.values() {
            require(token(selection.fault_id), "selection fault token")?;
        }
        Ok(())
    }
}

impl Network {
    pub(crate) fn check_scope_persist(
        &self,
        scope: &crate::oracle::service::Scope,
        resolve: &impl Fn(&str) -> std::result::Result<MachineId, String>,
    ) -> Result<()> {
        use crate::oracle::service::Scope;
        match scope {
            Scope::Machine { .. } => Ok(()),
            Scope::Connection { connection } => require(
                self.connection_members(connection),
                "connection scope members",
            ),
            Scope::Links { left, right }
            | Scope::DirectedLinks {
                from: left,
                to: right,
            } => {
                require(
                    !left.is_empty()
                        && !right.is_empty()
                        && left.windows(2).all(|w| w[0] < w[1])
                        && right.windows(2).all(|w| w[0] < w[1])
                        && left.iter().all(|n| right.binary_search(n).is_err()),
                    "network scope sides",
                )?;
                for name in left.iter().chain(right) {
                    let id =
                        resolve(name).map_err(|_| CodecError::Invalid("network scope member"))?;
                    require(self.attached(id), "network scope attachment")?;
                }
                Ok(())
            }
        }
    }
    pub(crate) fn check_frame_positions_persist(&self, max: Position) -> Result<()> {
        let mut previous = None;
        for f in &self.frames {
            require(
                f.position.boundary_index <= max.boundary_index
                    && f.position.completed_quanta <= max.completed_quanta
                    && f.position.completed_quanta <= f.position.boundary_index,
                "frame fact position",
            )?;
            if let Some(p) = previous {
                let p: Position = p;
                require(
                    p.at <= f.position.at
                        && p.boundary_index <= f.position.boundary_index
                        && p.completed_quanta <= f.position.completed_quanta,
                    "frame fact order",
                )?;
            }
            require(
                match f.fate {
                    FrameFate::Scheduled => f.due.is_some_and(|due| due >= f.position.at),
                    FrameFate::ReceiverDropped => f.due == Some(f.position.at),
                    _ => true,
                },
                "frame fact occurrence",
            )?;
            previous = Some(f.position);
        }
        Ok(())
    }
}
