//! Shared network configuration and the Driver-owned router for frames in flight.
//!
//! The router sees only simulation time. It decides each candidate delivery at
//! routing time, in the Driver's output publication order, and owns in-flight
//! frames, per-pair FIFO clocks and seeded loss/jitter streams. It is never part
//! of a machine snapshot; whole-Driver checkpoints retain it.
use super::{observation::FrameObservations, MachineId};
#[cfg(test)]
use super::{Position, Timestamp};
mod parity;
#[cfg(test)]
mod test_observer;
use crate::{
    definition::MacAddress,
    rng::{RngDomain, RngStreamExt, SeededRng},
};
pub use parity::*;
use parity::{Flight, Layer};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, time::Duration};

/// Largest combined latency and jitter: one hour.
const MAX_DELAY_NS: u64 = 3_600_000_000_000;
/// Parts per million in a certain loss.
const MAX_LOSS_PPM: u32 = 1_000_000;

// The shared network spans machines, so neither a machine
// definition nor a factory can express it; one validated constructor input keeps
// the minimum delay that bounds causal rounds fixed for the Driver's lifetime.
/// Delay and loss applied to every frame between attached machines.
///
/// Every frame is delayed by `latency` plus an additional seeded jitter in
/// `0..=jitter`, and lost at a seeded rate of about `loss_ppm` per million.
/// Both map one 64-bit draw (multiply-high and threshold comparison), so they
/// are not exactly uniform unless the span divides 2^64.
/// Latency is
/// strictly positive: it is the minimum delay that bounds how far machines may
/// execute before others can observe their frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkConfig {
    latency_ns: u64,
    jitter_ns: u64,
    loss_ppm: u32,
}

impl NetworkConfig {
    /// Accept a positive latency, jitter below latency, latency plus jitter at
    /// most one hour, and loss of at most 1_000_000 parts per million.
    pub fn new(
        latency: Duration,
        jitter: Duration,
        loss_ppm: u32,
    ) -> Result<Self, InvalidNetworkConfig> {
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).ok();
        let (Some(latency_ns), Some(jitter_ns)) = (nanos(latency), nanos(jitter)) else {
            return Err(InvalidNetworkConfig::DelayTooLong);
        };
        if latency_ns == 0 {
            return Err(InvalidNetworkConfig::ZeroLatency);
        }
        if jitter_ns >= latency_ns {
            return Err(InvalidNetworkConfig::JitterNotBelowLatency);
        }
        if latency_ns
            .checked_add(jitter_ns)
            .is_none_or(|delay| delay > MAX_DELAY_NS)
        {
            return Err(InvalidNetworkConfig::DelayTooLong);
        }
        if loss_ppm > MAX_LOSS_PPM {
            return Err(InvalidNetworkConfig::LossAboveCertain);
        }
        Ok(Self {
            latency_ns,
            jitter_ns,
            loss_ppm,
        })
    }

    /// The minimum delay of every frame.
    pub fn latency(self) -> Duration {
        Duration::from_nanos(self.latency_ns)
    }

    /// The largest additional seeded delay.
    pub fn jitter(self) -> Duration {
        Duration::from_nanos(self.jitter_ns)
    }

    /// The seeded loss probability in parts per million.
    pub fn loss_ppm(self) -> u32 {
        self.loss_ppm
    }

    pub(crate) fn latency_ns(self) -> u64 {
        self.latency_ns
    }
}

impl Default for NetworkConfig {
    /// One millisecond of latency, without jitter or loss.
    fn default() -> Self {
        Self {
            latency_ns: 1_000_000,
            jitter_ns: 0,
            loss_ppm: 0,
        }
    }
}

/// A rejected [`NetworkConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidNetworkConfig {
    /// Latency must be positive.
    ZeroLatency,
    /// Jitter must be below latency.
    JitterNotBelowLatency,
    /// Latency plus jitter exceeds one hour.
    DelayTooLong,
    /// Loss exceeds 1_000_000 parts per million.
    LossAboveCertain,
}

impl fmt::Display for InvalidNetworkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroLatency => "network latency must be positive",
            Self::JitterNotBelowLatency => "network jitter must be below latency",
            Self::DelayTooLong => "network latency plus jitter exceeds one hour",
            Self::LossAboveCertain => "network loss exceeds 1000000 ppm",
        })
    }
}
impl std::error::Error for InvalidNetworkConfig {}

/// A machine's network identity: its seed and name derive its pair streams.
#[derive(Clone)]
struct Member {
    name: String,
    seed: u64,
    mac: Option<MacAddress>,
    /// Frames this machine has routed, ordering same-arrival deliveries.
    sent: u64,
}

/// Seeded per-pair streams. Each is derived from the sender's machine seed and
/// the receiver's name in its own domain, so neither shares bytes with the
/// machine, clock-fault or other pair streams, nor shifts with admission order.
#[derive(Clone)]
struct Pair {
    loss: SeededRng,
    jitter: SeededRng,
    /// The latest arrival scheduled on this pair; arrivals strictly follow it.
    last: Option<u64>,
    last_depart: Option<u64>,
    faults: [SeededRng; 5],
}

/// Why a frame could not be routed. The Driver fails without retrying.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RouteError {
    /// The arrival time is not representable in u64 nanoseconds.
    ArrivalOverflow,
    /// The sender's frame sequence cannot order another candidate.
    SequenceOverflow,
}

/// One frame in flight, keyed by (arrival, sender, sender sequence, receiver).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Delivery {
    pub(crate) from: MachineId,
    pub(crate) sent: u64,
    pub(crate) to: MachineId,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct Network {
    config: NetworkConfig,
    members: Vec<Member>,
    pairs: BTreeMap<(MachineId, MachineId), Pair>,
    in_flight: BTreeMap<(u64, MachineId, u64, MachineId), Flight>,
    global: NetworkPolicy,
    baselines: BTreeMap<(String, String), NetworkPolicy>,
    layers: BTreeMap<u32, Layer>,
    connections: seedbox_engine_core::connections::Connections,
    queue_counts: BTreeMap<(MachineId, MachineId), QueueCounts>,
    counts: BTreeMap<(MachineId, MachineId), NetworkPairCounts>,
    /// Active partitions per directed pair; overlapping partitions stack.
    cuts: BTreeMap<(MachineId, MachineId), u32>,
    /// The directed pairs each active partition handle cut.
    partitions: BTreeMap<String, Vec<(MachineId, MachineId)>>,
}

/// The receiver's name, length-prefixed, mixed with the sender's seed.
fn pair_seed(sender_seed: u64, receiver: &str) -> u64 {
    let mut hash = Sha256::new();
    hash.update(b"seedbox-machine network pair v1\0");
    hash.update(sender_seed.to_le_bytes());
    hash.update((receiver.len() as u64).to_le_bytes());
    hash.update(receiver.as_bytes());
    u64::from_le_bytes(hash.finalize()[..8].try_into().expect("eight bytes"))
}

impl Network {
    pub(super) fn checkpoint_size(&self, machines: usize) -> bool {
        self.members.len() == machines
    }

    pub(crate) fn new(config: NetworkConfig) -> Self {
        Self {
            config,
            global: NetworkSetup::from(config).global(),
            baselines: BTreeMap::new(),
            layers: BTreeMap::new(),
            connections: seedbox_engine_core::connections::Connections::default(),
            queue_counts: BTreeMap::new(),
            counts: BTreeMap::new(),
            members: Vec::new(),
            pairs: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            cuts: BTreeMap::new(),
            partitions: BTreeMap::new(),
        }
    }

    /// Record machine `id`, admitted in identity order. `None` is unattached.
    pub(crate) fn admit(&mut self, id: MachineId, name: &str, seed: u64, mac: Option<MacAddress>) {
        debug_assert_eq!(id.0, self.members.len());
        debug_assert!(mac.is_none_or(|mac| self.members.iter().all(|m| m.mac != Some(mac))));
        self.members.push(Member {
            name: name.to_owned(),
            seed,
            mac,
            sent: 0,
        });
    }

    pub(crate) fn attached(&self, id: MachineId) -> bool {
        self.members.get(id.0).is_some_and(|m| m.mac.is_some())
    }

    pub(crate) fn latency_ns(&self) -> u64 {
        self.config.latency_ns()
    }

    /// Earliest arrival still in flight.
    pub(crate) fn next_arrival(&self) -> Option<u64> {
        self.in_flight.keys().next().map(|&(at, ..)| at)
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.in_flight.is_empty()
    }

    /// Cut both directions of every `left` × `right` pair under `handle`.
    /// False, changing nothing, for an already active handle.
    pub(crate) fn partition(
        &mut self,
        handle: String,
        left: &[MachineId],
        right: &[MachineId],
    ) -> bool {
        if self.partitions.contains_key(&handle) {
            return false;
        }
        let pairs: Vec<_> = left
            .iter()
            .flat_map(|&a| right.iter().flat_map(move |&b| [(a, b), (b, a)]))
            .collect();
        if pairs
            .iter()
            .any(|pair| self.cuts.get(pair).is_some_and(|&n| n == u32::MAX))
        {
            return false;
        }
        for &pair in &pairs {
            *self.cuts.entry(pair).or_default() += 1;
        }
        self.partitions.insert(handle, pairs);
        true
    }

    /// Restore only the links `handle` cut. False for an inactive handle.
    pub(crate) fn heal(&mut self, handle: &str) -> bool {
        let Some(pairs) = self.partitions.remove(handle) else {
            return false;
        };
        for pair in pairs {
            let count = self.cuts.get_mut(&pair).expect("counted cut");
            *count -= 1;
            if *count == 0 {
                self.cuts.remove(&pair);
            }
        }
        true
    }

    /// Route one frame `from` sent at simulation time `sent`, against the cuts
    /// in force now: the caller routes in publication and FIFO-turn order, not
    /// by timestamp alone. Broadcast reaches
    /// every other attached machine in identity order; unicast reaches its
    /// address's owner; other group addresses and frames to the sender itself
    /// reach nobody. Each candidate draws its pair's enabled loss and jitter
    /// streams once, in that order, whether or not it is lost; a partitioned
    /// candidate draws nothing and reserves no FIFO slot. An arrival no
    /// u64 nanosecond can represent, or an exhausted sender sequence, fails
    /// rather than being lost or clamped.
    #[cfg(test)]
    pub(crate) fn route(
        &mut self,
        from: MachineId,
        sent: u64,
        bytes: &[u8],
    ) -> Result<(), RouteError> {
        self.route_at(
            Position {
                at: Timestamp::from_nanos(sent),
                boundary_index: 0,
                completed_quanta: 0,
            },
            from,
            sent,
            bytes,
            &mut FrameObservations::default(),
        )
    }

    /// Remove every frame arriving at `now`, in (sender, sequence, receiver)
    /// order. No frame may remain due before `now`.
    /// The Driver's round boundary includes every arrival, so none is skipped.
    pub(crate) fn take_due(&mut self, now: u64) -> Vec<Delivery> {
        let mut due = Vec::new();
        while let Some(entry) = self.in_flight.first_entry() {
            let (arrival, from, _, to) = *entry.key();
            if arrival > now {
                break;
            }
            debug_assert_eq!(arrival, now, "network arrival passed without delivery");
            let frame = entry.remove();
            due.push(Delivery {
                from,
                to,
                sent: frame.sent,
                bytes: frame.bytes,
            });
        }
        due
    }

    /// Place a sender's frame sequence near exhaustion.
    #[cfg(test)]
    pub(crate) fn set_sequence(&mut self, from: MachineId, sent: u64) {
        self.members[from.0].sent = sent;
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> Vec<(u64, MachineId, MachineId, Vec<u8>)> {
        self.in_flight
            .iter()
            .map(|(&(at, from, _, to), bytes)| (at, from, to, bytes.bytes.clone()))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn pair_cursor(&self, from: MachineId, to: MachineId) -> Option<[u64; 2]> {
        let pair = self.pairs.get(&(from, to))?;
        let mut loss = pair.loss.clone();
        let mut jitter = pair.jitter.clone();
        Some([loss.next_u64(), jitter.next_u64()])
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod parity_tests;

mod evidence;
mod persist;

#[cfg(test)]
mod network_parity_tests;
