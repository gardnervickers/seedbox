//! Canonical pure policy types with Driver-owned streams and stopped routing.
use super::*;
use crate::driver::{Position, Timestamp};
pub use seedbox_control::{Connection, NetworkFaults};
use seedbox_engine_core::connections::{is_reset_frame, reset_frame};
pub use seedbox_engine_core::{
    links::{Policy as NetworkPolicy, QueueCounts},
    network_counters::PairCounts as NetworkPairCounts,
    network_faults::Counts as NetworkFaultCounts,
};
use std::collections::BTreeSet;

/// Collision-free identity of one directed counter pair within this Driver.
/// IDs are insertion identities retained by checkpoints, independent of names
/// and their punctuation. Ordering is sender ID then receiver ID. Resolve names
/// only for presentation; concatenated names are not pair identities.
// TODO: Re-evaluate: transport adapters own display spelling, not counter keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetworkPair {
    pub from: MachineId,
    pub to: MachineId,
}

/// One directed named baseline override; endpoints identify prepared definitions.
/// The entire policy replaces the global policy for this ordered pair (including
/// an explicit absence of bandwidth), rather than inheriting individual fields.
// TODO: Re-evaluate: shared links need named setup independent of machine
// construction order, including definitions that are spawned later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkPolicy {
    pub from: String,
    pub to: String,
    pub policy: NetworkPolicy,
}
/// Complete initial network policy, retaining the original causal latency floor.
/// Existing NetworkConfig and Driver constructors continue using no bandwidth,
/// no baselines and no frame observations. This value executes no work.
// TODO: Re-evaluate: per-pair/global queue policy cannot live in an individual
// machine definition or alter the existing Copy NetworkConfig boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkSetup {
    pub(super) config: NetworkConfig,
    pub(super) bandwidth_bps: Option<u64>,
    pub(super) queue_bound_ns: Option<u64>,
    pub(super) links: Vec<LinkPolicy>,
    pub(super) frame_capacity: usize,
    controller: Option<crate::driver::ControllerSetup>,
}
impl From<NetworkConfig> for NetworkSetup {
    fn from(config: NetworkConfig) -> Self {
        Self {
            config,
            bandwidth_bps: None,
            queue_bound_ns: None,
            links: Vec::new(),
            frame_capacity: 0,
            controller: None,
        }
    }
}
impl NetworkSetup {
    /// Validate all policies against config's latency floor, unique directed
    /// named pairs and nonempty distinct endpoints. Definition membership and
    /// interface attachment are checked by the Driver before factory work.
    // TODO: Re-evaluate: setup validates policy once without backend admission
    // or hidden defaults; the Driver still owns prepared-name membership.
    pub fn new(
        config: NetworkConfig,
        bandwidth_bps: Option<u64>,
        queue_bound_ns: Option<u64>,
        links: Vec<LinkPolicy>,
    ) -> Result<Self, String> {
        let s = Self {
            config,
            bandwidth_bps,
            queue_bound_ns,
            links,
            frame_capacity: 0,
            controller: None,
        };
        s.global().validate(config.latency_ns())?;
        let mut seen = BTreeSet::new();
        for l in &s.links {
            l.policy.validate(config.latency_ns())?;
            if l.from.is_empty()
                || l.to.is_empty()
                || l.from == l.to
                || !seen.insert((&l.from, &l.to))
            {
                return Err("invalid or duplicate baseline pair".into());
            }
        }
        Ok(s)
    }
    /// Opt into at most capacity undrained frame facts; zero disables collection.
    /// Overflow increments omitted, never changes routing or authoritative counts.
    /// Both the limit and undrained state survive checkpoints.
    // TODO: Re-evaluate: packet observation must be bounded and separate from
    // dense committed EventKind sequencing and deterministic witness content.
    pub fn with_frame_capacity(mut self, capacity: usize) -> Self {
        self.frame_capacity = capacity;
        self
    }
    /// Opt into service controller semantics. Explicit None disables all guest
    /// callers. Driver validates a present identity as its named oracle before
    /// factory work. Latency is at least the network causal bound. Omission
    /// retains SDK timing and authorization byte-for-byte, with no RNG draws.
    ///
    /// Calls join the existing FIFO at observed+latency; replies at effect+latency
    /// (or rejection decision+latency). Existing same-time host inputs remain
    /// before later-enqueued calls; effects and matched triggers append at their
    /// current FIFO turn. Frames and timers retain their ordinary ordering.
    /// Unauthorized guest outputs poison the Driver. Guest targets cannot name
    /// the controller, but host input authority/timing is unchanged. A reply is
    /// pinned to its original boot and discarded if that boot has ended.
    pub fn with_controller(
        mut self,
        controller: Option<super::MachineId>,
        latency: std::time::Duration,
    ) -> Result<Self, String> {
        self.controller = Some(crate::driver::ControllerSetup::new(
            controller,
            latency,
            self.config.latency_ns(),
        )?);
        Ok(self)
    }
    /// Configured service authority, or the unmodified SDK contract.
    pub fn controller_setup(&self) -> Option<crate::driver::ControllerSetup> {
        self.controller
    }
    pub(super) fn global(&self) -> NetworkPolicy {
        NetworkPolicy {
            latency_ns: self.config.latency_ns,
            jitter_ns: self.config.jitter_ns,
            loss_ppm: self.config.loss_ppm.into(),
            bandwidth_bps: self.bandwidth_bps,
            queue_bound_ns: self.queue_bound_ns,
        }
    }
}
/// Counts returned by a selection heal, sampled when the heal is admitted.
/// `matched` includes matching frames masked by a partition; `dropped` counts
/// only frames for which this selection won the loss decision.
// TODO: Re-evaluate: admission-time snapshots must not drift while a heal waits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SelectionCounts {
    pub matched: u64,
    pub dropped: u64,
}
/// Typed committed network heal result, also used verbatim in guest replies.
/// Shape counts are sampled when removal applies; selection counts at admission.
/// Disabled shape features are absent, enabled features may have zero counts.
/// Selection counts are exclusive of queue/fault counts.
// TODO: Re-evaluate: service receipts need counts without parsing free-form
// details or adding another event that shifts existing dense event sequences.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct NetworkSettlement {
    #[serde(rename = "queue_counts", skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueCounts>,
    #[serde(
        rename = "network_fault_counts",
        skip_serializing_if = "Option::is_none"
    )]
    pub faults: Option<NetworkFaultCounts>,
    #[serde(flatten)]
    pub selection: Option<SelectionCounts>,
}
/// Committed network observation, never an EventKind or witness contribution.
/// Scheduled proves enqueueing, not delivery. A later ReceiverDropped can refer
/// to that same frame; selection, ordinary loss and burst loss share LossDropped.
// TODO: Re-evaluate: packet facts and confirmed receive counters are independent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFate {
    Scheduled,
    PartitionDropped,
    LossDropped,
    QueueDropped,
    ReceiverDropped,
}
/// A route candidate or a later exited-receiver drop. bytes is the length of
/// the actual offered bytes (including a reset substitution), not a payload copy.
/// Position is the route commit, or the delivery-time position for ReceiverDropped.
/// Duplicate copies have their own scheduled fact and due time.
// TODO: Re-evaluate: bounded facts intentionally do not retain packet bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameFact {
    pub position: Position,
    pub from: MachineId,
    pub to: MachineId,
    pub sent: Timestamp,
    pub due: Option<Timestamp>,
    pub bytes: usize,
    pub fate: FrameFate,
}
/// One bounded observation drain. omitted counts facts not retained since the
/// last drain; authoritative per-pair counters do not depend on observation.
/// Omitted saturates at u64::MAX. Initially disabled observation returns an
/// empty drain with zero omissions. Clearing/shrinking preserves omissions for
/// the next drain; draining resets only facts and their omission count.
// TODO: Re-evaluate: omission is transport loss, never simulation packet loss.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameFacts {
    pub facts: Vec<FrameFact>,
    pub omitted: u64,
}
#[derive(Clone)]
pub(super) struct Layer {
    pub(super) handle: String,
    pub(super) pairs: BTreeSet<(MachineId, MachineId)>,
    pub(super) policy: NetworkPolicy,
    pub(super) faults: NetworkFaults,
    pub(super) queue: QueueCounts,
    pub(super) counts: NetworkFaultCounts,
    pub(super) burst: BTreeMap<(MachineId, MachineId), u64>,
}
#[derive(Clone)]
pub(super) struct Flight {
    pub(super) sent: u64,
    pub(super) bytes: Vec<u8>,
}
pub(super) const FAULT_DOMAINS: [RngDomain; 5] = [
    RngDomain::NetworkReorder,
    RngDomain::NetworkDuplicate,
    RngDomain::NetworkBurst,
    RngDomain::NetworkSpike,
    RngDomain::NetworkReset,
];
fn choose(rng: &mut SeededRng, ppm: u64) -> bool {
    ppm != 0 && u128::from(rng.next_u64()) * 1_000_000 < u128::from(ppm) << 64
}
fn below(rng: &mut SeededRng, n: u64) -> u64 {
    // Same rejection mapping as canonical Rng::below, but from the Driver byte
    // stream's own next_u64 (never an assumption about fill_bytes width).
    let zone = u64::MAX - u64::MAX % n;
    loop {
        let value = rng.next_u64();
        if value < zone {
            return value % n;
        }
    }
}
impl Pair {
    pub(super) fn new(seed: u64) -> Self {
        Self {
            loss: SeededRng::derived(seed, RngDomain::NetworkLoss),
            jitter: SeededRng::derived(seed, RngDomain::NetworkJitter),
            last: None,
            last_depart: None,
            faults: FAULT_DOMAINS.map(|d| SeededRng::derived(seed, d)),
        }
    }
}
impl Network {
    pub(crate) fn from_setup(setup: NetworkSetup) -> Self {
        let mut n = Self::new(setup.config);
        n.global = setup.global();
        n.frame_capacity = setup.frame_capacity;
        n.baselines = setup
            .links
            .into_iter()
            .map(|l| ((l.from, l.to), l.policy))
            .collect();
        n
    }
    pub(crate) fn validate_setup(
        setup: &NetworkSetup,
        definitions: impl Iterator<
            Item = impl std::borrow::Borrow<crate::definition::MachineDefinition>,
        >,
    ) -> Result<(), String> {
        let members: BTreeSet<_> = definitions
            .filter_map(|d| {
                let d = d.borrow();
                d.network.as_ref().map(|_| d.name.clone())
            })
            .collect();
        for l in &setup.links {
            if !members.contains(&l.from) || !members.contains(&l.to) {
                return Err("baseline endpoint is unknown or unattached".into());
            }
        }
        Ok(())
    }
    pub(super) fn fact(&mut self, fact: FrameFact) {
        if self.frame_capacity == 0 {
            return;
        }
        if self.frames.len() < self.frame_capacity {
            self.frames.push_back(fact);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
    pub(crate) fn set_frame_capacity(&mut self, capacity: usize) {
        self.frame_capacity = capacity;
        if self.frames.len() > capacity {
            let discarded = self.frames.len() - capacity;
            self.frames.truncate(capacity);
            self.omitted = self.omitted.saturating_add(discarded as u64);
        }
    }

    pub(crate) fn drain_frames(&mut self) -> FrameFacts {
        FrameFacts {
            facts: self.frames.drain(..).collect(),
            omitted: std::mem::take(&mut self.omitted),
        }
    }
    pub(crate) fn connection_members(&self, c: &Connection) -> bool {
        c.from != c.to
            && c.src_port != 0
            && c.dst_port != 0
            && c.incarnation != 0
            && [&c.from, &c.to].iter().all(|name| {
                self.members
                    .iter()
                    .any(|m| m.mac.is_some() && m.name.as_str() == name.as_str())
            })
    }
    pub(crate) fn connection_names(&self) -> Vec<String> {
        self.members.iter().map(|m| m.name.clone()).collect()
    }
    pub(crate) fn connections(&self, from: &str, to: &str, after: u64) -> Vec<Connection> {
        self.connections
            .list(from, to, after, &self.connection_names())
    }
    pub(crate) fn selection_available(&self, c: &Connection) -> bool {
        self.connections.selections.len() < seedbox_engine_core::connections::CAPACITY
            && self.connections.available(c, &self.connection_names())
    }
    pub(crate) fn select(&mut self, id: u32, handle: String, c: Connection) -> Result<(), String> {
        self.connections
            .select(id, handle, c, &self.connection_names())
    }
    pub(crate) fn selection_counts(&self, handle: &str) -> Option<NetworkSettlement> {
        self.connections
            .selections
            .get(handle)
            .map(|s| NetworkSettlement {
                selection: Some(SelectionCounts {
                    matched: s.matched,
                    dropped: s.dropped,
                }),
                ..NetworkSettlement::default()
            })
    }
    pub(crate) fn unselect(&mut self, handle: &str) -> bool {
        self.connections.selections.remove(handle).is_some()
    }
    pub(crate) fn counts(&self) -> BTreeMap<NetworkPair, NetworkPairCounts> {
        self.counts
            .iter()
            .map(|(&(from, to), &c)| (NetworkPair { from, to }, c))
            .collect()
    }
    /// Compatibility-only guest inspect projection, using the unchanged canonical
    /// legacy display helper. Its concatenated names are not authoritative pair
    /// identities for arbitrary Driver names; host views use counts() and IDs.
    pub(crate) fn legacy_counts(&self) -> serde_json::Value {
        use seedbox_engine_core::network_counters as c;
        let counts = self
            .counts
            .iter()
            .map(|(&(a, b), n)| {
                let mut row = [0; c::OUTCOMES];
                row[c::DELIVERED] = n.delivered;
                row[c::PARTITION_DROPPED] = n.partition_dropped;
                row[c::LOSS_DROPPED] = n.loss_dropped;
                row[c::QUEUE_DROPPED] = n.queue_dropped;
                row[c::RECEIVER_DROPPED] = n.receiver_dropped;
                ((a.0, b.0), row)
            })
            .collect();
        c::snapshot_value(|id| self.members[id].name.as_str(), &counts)
    }
    pub(crate) fn shape(
        &mut self,
        id: u32,
        handle: String,
        from: &[MachineId],
        to: &[MachineId],
        policy: NetworkPolicy,
        faults: NetworkFaults,
    ) -> bool {
        if self.layers.contains_key(&id) || self.layers.values().any(|l| l.handle == handle) {
            return false;
        }
        self.layers.insert(
            id,
            Layer {
                handle,
                pairs: from
                    .iter()
                    .flat_map(|&a| to.iter().map(move |&b| (a, b)))
                    .collect(),
                policy,
                faults,
                queue: QueueCounts::default(),
                counts: NetworkFaultCounts::default(),
                burst: BTreeMap::new(),
            },
        );
        true
    }
    pub(crate) fn unshape(&mut self, handle: &str) -> Option<NetworkSettlement> {
        let id = self
            .layers
            .iter()
            .find(|(_, l)| l.handle == handle)
            .map(|(&id, _)| id)?;
        let l = self.layers.remove(&id)?;
        Some(NetworkSettlement {
            queue: l.policy.bandwidth_bps.map(|_| l.queue),
            faults: (!l.faults.is_off()).then_some(l.counts),
            selection: None,
        })
    }
    fn layer_id(&self, pair: (MachineId, MachineId)) -> Option<u32> {
        self.layers
            .iter()
            .rev()
            .find(|(_, l)| l.pairs.contains(&pair))
            .map(|(&id, _)| id)
    }
    fn policy(&self, pair: (MachineId, MachineId), layer: Option<u32>) -> NetworkPolicy {
        layer
            .map(|id| self.layers[&id].policy)
            .or_else(|| {
                self.baselines
                    .get(&(
                        self.members[pair.0 .0].name.clone(),
                        self.members[pair.1 .0].name.clone(),
                    ))
                    .copied()
            })
            .unwrap_or(self.global)
    }
    pub(crate) fn route_at(
        &mut self,
        position: Position,
        from: MachineId,
        sent: u64,
        bytes: &[u8],
    ) -> Result<(), RouteError> {
        let destination: [u8; 6] = bytes[..6].try_into().expect("bounded frame");
        let receivers: Vec<_> = if destination == [255; 6] {
            (0..self.members.len())
                .map(MachineId)
                .filter(|&id| id != from && self.attached(id))
                .collect()
        } else {
            self.members
                .iter()
                .position(|m| m.mac.is_some_and(|m| m.octets() == destination))
                .map(MachineId)
                .filter(|&id| id != from)
                .into_iter()
                .collect()
        };
        for to in receivers {
            let pair_key = (from, to);
            // Classification and selected-match counts precede cut masking.
            let identity = self.connections.observe(from.0, to.0, bytes);
            let cut = self.cuts.contains_key(&pair_key);
            let selected = self.connections.drops(identity, cut, |_| {});
            let mut fact = FrameFact {
                position,
                from,
                to,
                sent: Timestamp::from_nanos(sent),
                due: None,
                bytes: bytes.len(),
                fate: FrameFate::PartitionDropped,
            };
            if cut {
                self.counts.entry(pair_key).or_default().partition_dropped += 1;
                self.fact(fact);
                continue;
            }
            let sequence = self.members[from.0].sent;
            self.members[from.0].sent = sequence
                .checked_add(1)
                .ok_or(RouteError::SequenceOverflow)?;
            let layer = self.layer_id(pair_key);
            let policy = self.policy(pair_key, layer);
            let seed = pair_seed(self.members[from.0].seed, &self.members[to.0].name);
            let p = self
                .pairs
                .entry(pair_key)
                .or_insert_with(|| Pair::new(seed));
            let loss = choose(&mut p.loss, policy.loss_ppm);
            let jitter = if policy.jitter_ns == 0 {
                0
            } else {
                ((u128::from(p.jitter.next_u64()) * (u128::from(policy.jitter_ns) + 1)) >> 64)
                    as u64
            };
            if selected || loss {
                self.counts.entry(pair_key).or_default().loss_dropped += 1;
                fact.fate = FrameFate::LossDropped;
                self.fact(fact);
                continue;
            }
            if let Some(id) = layer {
                let l = self.layers.get_mut(&id).unwrap();
                if !l.faults.is_off() {
                    l.counts.matched += 1;
                }
                if l.faults.burst_loss_ppm != 0 {
                    let remaining = l.burst.entry(pair_key).or_default();
                    let dropped = if *remaining > 0 {
                        *remaining -= 1;
                        true
                    } else if choose(&mut p.faults[2], l.faults.burst_loss_ppm) {
                        *remaining = l.faults.burst_length - 1;
                        true
                    } else {
                        false
                    };
                    if dropped {
                        l.counts.burst_dropped += 1;
                        self.counts.entry(pair_key).or_default().loss_dropped += 1;
                        fact.fate = FrameFate::LossDropped;
                        self.fact(fact);
                        continue;
                    }
                }
            }
            let depart = if let Some(rate) = policy.bandwidth_bps {
                let service = (bytes.len() as u128 * 8_000_000_000).div_ceil(u128::from(rate));
                let depart = u64::try_from(service)
                    .ok()
                    .and_then(|s| sent.max(p.last_depart.unwrap_or(0)).checked_add(s));
                let delay = depart.map(|at| at - sent).unwrap_or(u64::MAX);
                let dropped = depart.is_none()
                    || policy.queue_bound_ns.is_some_and(|b| delay > b)
                    || depart
                        .and_then(|at| at.checked_add(policy.latency_ns)?.checked_add(jitter))
                        .is_none()
                    || p.last == Some(u64::MAX);
                let record = |c: &mut QueueCounts| {
                    c.matched += 1;
                    c.queue_dropped += u64::from(dropped);
                    c.delayed_max_ns = c.delayed_max_ns.max(delay);
                };
                record(self.queue_counts.entry(pair_key).or_default());
                if let Some(id) = layer {
                    record(&mut self.layers.get_mut(&id).unwrap().queue);
                }
                if dropped {
                    self.counts.entry(pair_key).or_default().queue_dropped += 1;
                    fact.fate = FrameFate::QueueDropped;
                    self.fact(fact);
                    continue;
                }
                let depart = depart.unwrap();
                p.last_depart = Some(depart);
                depart
            } else {
                sent
            };
            let arrival = depart
                .checked_add(policy.latency_ns)
                .and_then(|at| at.checked_add(jitter))
                .and_then(|at| match p.last {
                    Some(last) => last.checked_add(1).map(|next| at.max(next)),
                    None => Some(at),
                })
                .ok_or(RouteError::ArrivalOverflow)?;
            p.last = Some(arrival);
            let mut at = arrival;
            let mut delivered = bytes.to_vec();
            let mut duplicate = false;
            if let Some(id) = layer {
                let l = self.layers.get_mut(&id).unwrap();
                let f = l.faults;
                let mut delay = 0;
                if choose(&mut p.faults[0], f.reorder_ppm) {
                    delay = below(&mut p.faults[0], f.reorder_delay_ns) + 1;
                    l.counts.reordered += 1;
                }
                if choose(&mut p.faults[3], f.latency_spike_ppm) {
                    delay += f.latency_spike_ns;
                    l.counts.spiked += 1;
                }
                l.counts.delayed_max_ns = l.counts.delayed_max_ns.max(delay);
                if let Some(reset) = if f.tcp_reset_ppm != 0 {
                    reset_frame(bytes)
                } else {
                    None
                } {
                    if choose(&mut p.faults[4], f.tcp_reset_ppm) {
                        delivered = reset;
                        l.counts.tcp_reset += 1;
                    }
                }
                at = arrival.saturating_add(delay);
                if choose(&mut p.faults[1], f.duplicate_ppm) && at.checked_add(1).is_some() {
                    duplicate = true;
                    l.counts.duplicated += 1;
                }
            }
            if is_reset_frame(&delivered) {
                self.connections.observe(from.0, to.0, &delivered);
            }
            fact.fate = FrameFate::Scheduled;
            fact.due = Some(Timestamp::from_nanos(at));
            fact.bytes = delivered.len();
            self.fact(fact.clone());
            if duplicate {
                let due = at + 1;
                fact.due = Some(Timestamp::from_nanos(due));
                self.fact(fact);
                self.in_flight.insert(
                    (due, from, sequence, to),
                    Flight {
                        sent,
                        bytes: delivered.clone(),
                    },
                );
            }
            self.in_flight.insert(
                (at, from, sequence, to),
                Flight {
                    sent,
                    bytes: delivered,
                },
            );
        }
        Ok(())
    }
    pub(crate) fn delivery_result(
        &mut self,
        position: Position,
        from: MachineId,
        to: MachineId,
        sent: u64,
        bytes: usize,
        dropped: bool,
    ) {
        let counts = self.counts.entry((from, to)).or_default();
        if dropped {
            counts.receiver_dropped += 1;
            self.fact(FrameFact {
                position,
                from,
                to,
                sent: Timestamp::from_nanos(sent),
                due: Some(position.at),
                bytes,
                fate: FrameFate::ReceiverDropped,
            });
        } else {
            counts.delivered += 1;
        }
    }
    pub(crate) fn observation_mark(&self) -> (usize, u64) {
        (self.frames.len(), self.omitted)
    }
    pub(crate) fn discard_uncommitted_facts(&mut self, mark: (usize, u64)) {
        self.frames.truncate(mark.0);
        self.omitted = mark.1;
    }
}
