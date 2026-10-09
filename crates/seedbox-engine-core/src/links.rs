//! Directed, send-time link decisions. No host, device or scheduler dependencies.
use crate::network_faults::{self, Counts, Streams};
use crate::rng::Rng;
use seedbox_control::NetworkFaults;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub type Pair = (usize, usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
    /// None is infinite bandwidth; measured over the transmitted Ethernet bytes.
    pub bandwidth_bps: Option<u64>,
    /// Maximum send-to-departure time, including this frame's serialization.
    pub queue_bound_ns: Option<u64>,
}
impl Policy {
    pub fn validate_bandwidth(self) -> Result<(), String> {
        if self.bandwidth_bps == Some(0) {
            return Err("bandwidth_bps must be positive".into());
        }
        if self.queue_bound_ns.is_some() && self.bandwidth_bps.is_none() {
            return Err("queue_bound_ns requires bandwidth_bps".into());
        }
        Ok(())
    }
    pub fn validate(self, floor: u64) -> Result<(), String> {
        if self.latency_ns < floor {
            return Err("latency_ns below global latency".into());
        }
        if self.jitter_ns >= self.latency_ns {
            return Err("jitter_ns must be less than latency_ns".into());
        }
        if self
            .latency_ns
            .checked_add(self.jitter_ns)
            .is_none_or(|n| n > 3_600_000_000_000)
        {
            return Err("latency_ns + jitter_ns exceeds one hour".into());
        }
        if self.loss_ppm > 1_000_000 {
            return Err("loss_ppm exceeds 1000000".into());
        }
        self.validate_bandwidth()
    }
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueueCounts {
    pub matched: u64,
    pub queue_dropped: u64,
    pub delayed_max_ns: u64,
}
impl QueueCounts {
    fn record(&mut self, delay: u64, dropped: bool) {
        self.matched += 1;
        self.queue_dropped += u64::from(dropped);
        self.delayed_max_ns = self.delayed_max_ns.max(delay);
    }
    fn witness(self, h: &mut Sha256) {
        for n in [self.matched, self.queue_dropped, self.delayed_max_ns] {
            h.update(n.to_le_bytes());
        }
    }
}
#[derive(Clone)]
pub struct Layer {
    pub handle: String,
    pub pairs: BTreeSet<Pair>,
    pub policy: Policy,
    pub queue_counts: QueueCounts,
    pub network_faults: NetworkFaults,
    pub network_counts: Counts,
    burst_remaining: BTreeMap<Pair, u64>,
}
#[derive(Clone)]
pub struct Links {
    pub global: Policy,
    pub baselines: BTreeMap<Pair, Policy>,
    pub layers: BTreeMap<u32, Layer>,
    pub last: BTreeMap<Pair, u64>,
    pub last_depart: BTreeMap<Pair, u64>,
    pub queue_counts: BTreeMap<Pair, QueueCounts>,
    /// Retained after healing, including a bandwidth shape with no traffic.
    bandwidth_used: bool,
    /// A shape layer has been applied at some point in this run.
    pub shaped: bool,
    loss: BTreeMap<Pair, Rng>,
    jitter: BTreeMap<Pair, Rng>,
    network_streams: Streams,
    network_used: bool,
    /// Per-machine `clock/<machine>` streams for seeded clock skew and drift;
    /// drawn only when a seeded skew or drift is applied.
    clock: BTreeMap<usize, Rng>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Cut,
    Lost,
    QueueDropped,
    Deliver(u64),
}
impl Links {
    pub fn new(
        seed: u64,
        names: &[String],
        global: Policy,
        baselines: BTreeMap<Pair, Policy>,
    ) -> Result<Self, String> {
        // Preserve the existing global setting's bounds. The one-hour cap is
        // for overrides and shape layers; the whole-run floor is unchanged.
        if global.jitter_ns >= global.latency_ns || global.loss_ppm > 1_000_000 {
            return Err("invalid global link policy".into());
        }
        global.validate_bandwidth()?;
        for (&(a, b), policy) in &baselines {
            if a >= names.len() || b >= names.len() {
                return Err("invalid baseline pair".into());
            }
            policy.validate(global.latency_ns)?;
        }
        let bandwidth_used =
            global.bandwidth_bps.is_some() || baselines.values().any(|p| p.bandwidth_bps.is_some());
        let mut links = Self {
            global,
            baselines,
            layers: BTreeMap::new(),
            last: BTreeMap::new(),
            last_depart: BTreeMap::new(),
            queue_counts: BTreeMap::new(),
            bandwidth_used,
            shaped: false,
            loss: BTreeMap::new(),
            jitter: BTreeMap::new(),
            network_streams: Streams::default(),
            network_used: false,
            clock: BTreeMap::new(),
        };
        links.reseed(seed, names);
        Ok(links)
    }
    pub fn from_config(seed: u64, names: &[String], net: &Value) -> Result<Self, String> {
        let latency_ns = net
            .get("latency_ns")
            .map(|v| v.as_u64().ok_or("invalid network.latency_ns"))
            .transpose()?
            .unwrap_or(1_000_000);
        let jitter_ns = net
            .get("jitter_ns")
            .map(|v| v.as_u64().ok_or("invalid network.jitter_ns"))
            .transpose()?
            .unwrap_or(0);
        let global = Policy {
            latency_ns,
            jitter_ns,
            loss_ppm: 0,
            bandwidth_bps: optional_u64(net, "bandwidth_bps")?,
            queue_bound_ns: optional_u64(net, "queue_bound_ns")?,
        };
        let mut baselines = BTreeMap::new();
        if let Some(raw) = net.get("links") {
            for (i, link) in raw
                .as_array()
                .ok_or("network.links must be a list")?
                .iter()
                .enumerate()
            {
                let parse = || -> Result<(Pair, Policy), String> {
                    let index = |key| {
                        names
                            .iter()
                            .position(|n| Some(n.as_str()) == link[key].as_str())
                            .ok_or("unknown link machine")
                    };
                    let policy = Policy {
                        latency_ns: link["latency_ns"].as_u64().ok_or("latency_ns required")?,
                        jitter_ns: link
                            .get("jitter_ns")
                            .map(|v| v.as_u64().ok_or("invalid jitter_ns"))
                            .transpose()?
                            .unwrap_or(0),
                        loss_ppm: link
                            .get("loss_ppm")
                            .map(|v| v.as_u64().ok_or("invalid loss_ppm"))
                            .transpose()?
                            .unwrap_or(0),
                        bandwidth_bps: optional_u64(link, "bandwidth_bps")?,
                        queue_bound_ns: optional_u64(link, "queue_bound_ns")?,
                    };
                    policy.validate(global.latency_ns)?;
                    Ok(((index("from")?, index("to")?), policy))
                };
                let (pair, policy) = parse().map_err(|e| format!("network.links[{i}]: {e}"))?;
                if baselines.insert(pair, policy).is_some() {
                    return Err(format!("network.links[{i}]: duplicate pair"));
                }
            }
        }
        Self::new(seed, names, global, baselines)
    }
    pub fn reseed(&mut self, seed: u64, names: &[String]) {
        self.reseed_branch(seed, None, names);
    }
    /// Preserve sampled arrivals, baselines and layers; only future draws change.
    pub fn reseed_branch(&mut self, seed: u64, identity: Option<&str>, names: &[String]) {
        self.network_streams.reseed(seed, identity, names);
        self.loss.clear();
        self.jitter.clear();
        self.clock = (0..names.len())
            .map(|m| {
                (
                    m,
                    Rng::branch(seed, identity, &format!("clock/{}", names[m])),
                )
            })
            .collect();
        for a in 0..names.len() {
            for b in 0..names.len() {
                if a != b {
                    self.loss.insert(
                        (a, b),
                        Rng::branch(
                            seed,
                            identity,
                            &format!("link-loss/{}/{}", names[a], names[b]),
                        ),
                    );
                    self.jitter.insert(
                        (a, b),
                        Rng::branch(seed, identity, &format!("link/{}/{}", names[a], names[b])),
                    );
                }
            }
        }
    }
    /// A signed clock offset (ns) or rate (ppb) drawn uniformly from
    /// [-max, max] on machine `m`'s stream.
    pub fn clock_offset(&mut self, m: usize, max: u64) -> Result<i64, String> {
        let span = max
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .filter(|_| i64::try_from(max).is_ok())
            .ok_or("clock skew bound overflow")?;
        let rng = self.clock.get_mut(&m).ok_or("unknown clock machine")?;
        Ok(rng.below(span) as i64 - max as i64)
    }
    /// Advance both named streams for deterministic diagnostics and acceptance.
    #[doc(hidden)]
    pub fn stream_sample(&mut self, pair: Pair) -> (u64, u64) {
        (
            self.loss.get_mut(&pair).unwrap().next(),
            self.jitter.get_mut(&pair).unwrap().next(),
        )
    }
    /// The run has baseline overrides or has applied a shape layer.
    pub fn in_use(&self) -> bool {
        self.shaped || !self.baselines.is_empty() || self.bandwidth_used
    }
    pub fn policy(&self, pair: Pair) -> Policy {
        self.layers
            .values()
            .rev()
            .find(|layer| layer.pairs.contains(&pair))
            .map(|l| l.policy)
            .or_else(|| self.baselines.get(&pair).copied())
            .unwrap_or(self.global)
    }
    pub fn shape(
        &mut self,
        id: u32,
        handle: String,
        pairs: BTreeSet<Pair>,
        policy: Policy,
    ) -> Result<(), String> {
        self.shape_with_faults(id, handle, pairs, policy, NetworkFaults::default())
    }
    pub fn shape_with_faults(
        &mut self,
        id: u32,
        handle: String,
        pairs: BTreeSet<Pair>,
        policy: Policy,
        network_faults: NetworkFaults,
    ) -> Result<(), String> {
        policy.validate(self.global.latency_ns)?;
        network_faults.validate(policy.latency_ns, policy.jitter_ns)?;
        if self.layers.contains_key(&id) || self.layers.values().any(|l| l.handle == handle) {
            return Err("duplicate active shape".into());
        }
        self.shaped = true;
        self.bandwidth_used |= policy.bandwidth_bps.is_some();
        self.network_used |= !network_faults.is_off();
        self.layers.insert(
            id,
            Layer {
                handle,
                pairs,
                policy,
                queue_counts: QueueCounts::default(),
                network_faults,
                network_counts: Counts::default(),
                burst_remaining: BTreeMap::new(),
            },
        );
        Ok(())
    }
    pub fn heal(&mut self, handle: &str) -> bool {
        let id = self
            .layers
            .iter()
            .find(|(_, l)| l.handle == handle)
            .map(|(&id, _)| id);
        id.is_some_and(|id| self.layers.remove(&id).is_some())
    }
    /// A selected non-cut candidate consumes both draws without advancing FIFO.
    pub fn selected_drop(&mut self, src: usize, dst: usize) {
        self.loss.get_mut(&(src, dst)).unwrap().next();
        self.jitter.get_mut(&(src, dst)).unwrap().next();
    }
    pub fn decide(
        &mut self,
        src: usize,
        dst: usize,
        send: u64,
        bytes: usize,
        cut: bool,
    ) -> Outcome {
        if cut {
            return Outcome::Cut;
        }
        let pair = (src, dst);
        let policy = self.policy(pair);
        let loss = self.loss.get_mut(&pair).unwrap().next();
        let jitter = self.jitter.get_mut(&pair).unwrap().next();
        let jitter = ((jitter as u128 * (policy.jitter_ns as u128 + 1)) >> 64) as u64;
        if (loss as u128) * 1_000_000 < (policy.loss_ppm as u128) << 64 {
            return Outcome::Lost;
        }
        // Existing loss/jitter draws precede every new stream. Bursts reserve
        // neither the serialization clock nor the FIFO arrival clock.
        if let Some(layer) = self
            .layers
            .values_mut()
            .rev()
            .find(|l| l.pairs.contains(&pair))
        {
            let f = layer.network_faults;
            if !f.is_off() {
                layer.network_counts.matched += 1;
            }
            if f.burst_loss_ppm != 0 {
                let remaining = layer.burst_remaining.entry(pair).or_default();
                if *remaining > 0 {
                    *remaining -= 1;
                } else if self
                    .network_streams
                    .choose(pair, network_faults::BURST, f.burst_loss_ppm)
                {
                    *remaining = f.burst_length - 1;
                } else {
                    return self.finish_decision(pair, send, bytes, policy, jitter);
                }
                layer.network_counts.burst_dropped += 1;
                return Outcome::Lost;
            }
        }
        self.finish_decision(pair, send, bytes, policy, jitter)
    }
    fn finish_decision(
        &mut self,
        pair: Pair,
        send: u64,
        bytes: usize,
        policy: Policy,
        jitter: u64,
    ) -> Outcome {
        // Bandwidth uses no randomness, and is evaluated after the established
        // loss/jitter draws. Dropped candidates reserve neither clock.
        let depart = if let Some(rate) = policy.bandwidth_bps {
            let service = (bytes as u128 * 8_000_000_000).div_ceil(rate as u128);
            let depart = u64::try_from(service).ok().and_then(|service| {
                send.max(self.last_depart.get(&pair).copied().unwrap_or(0))
                    .checked_add(service)
            });
            let delay = depart.map(|t| t - send).unwrap_or(u64::MAX);
            let dropped = depart.is_none()
                || policy.queue_bound_ns.is_some_and(|bound| delay > bound)
                || depart
                    .and_then(|t| t.checked_add(policy.latency_ns)?.checked_add(jitter))
                    .is_none()
                || self.last.get(&pair) == Some(&u64::MAX);
            self.queue_counts
                .entry(pair)
                .or_default()
                .record(delay, dropped);
            if let Some(layer) = self
                .layers
                .values_mut()
                .rev()
                .find(|l| l.pairs.contains(&pair))
            {
                layer.queue_counts.record(delay, dropped);
            }
            if dropped {
                return Outcome::QueueDropped;
            }
            let depart = depart.expect("checked departure");
            self.last_depart.insert(pair, depart);
            depart
        } else {
            send
        };
        let arrival = (depart + policy.latency_ns + jitter)
            .max(self.last.get(&pair).copied().unwrap_or(0) + 1);
        self.last.insert(pair, arrival);
        Outcome::Deliver(arrival)
    }
    /// Receive-side injection at the retained frame deadline. Added delays are
    /// outside the base FIFO clock so a later frame can overtake a held frame.
    /// A duplicate is one copy a nanosecond later, without another send
    /// decision or serialization charge. Healing never edits these queued bytes.
    pub fn deliveries(&mut self, pair: Pair, arrival: u64, frame: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let Some(layer) = self
            .layers
            .values_mut()
            .rev()
            .find(|l| l.pairs.contains(&pair))
        else {
            return vec![(arrival, frame.to_vec())];
        };
        let f = layer.network_faults;
        let mut delay = 0;
        if self
            .network_streams
            .choose(pair, network_faults::REORDER, f.reorder_ppm)
        {
            delay += self.network_streams.delay(pair, f.reorder_delay_ns);
            layer.network_counts.reordered += 1;
        }
        if self
            .network_streams
            .choose(pair, network_faults::SPIKE, f.latency_spike_ppm)
        {
            delay += f.latency_spike_ns;
            layer.network_counts.spiked += 1;
        }
        layer.network_counts.delayed_max_ns = layer.network_counts.delayed_max_ns.max(delay);
        let reset = if f.tcp_reset_ppm != 0 {
            crate::connections::reset_frame(frame)
        } else {
            None
        };
        let bytes = if let Some(reset) = reset.filter(|_| {
            self.network_streams
                .choose(pair, network_faults::RESET, f.tcp_reset_ppm)
        }) {
            layer.network_counts.tcp_reset += 1;
            reset
        } else {
            frame.to_vec()
        };
        let at = arrival.saturating_add(delay);
        let mut deliveries = vec![(at, bytes)];
        if self
            .network_streams
            .choose(pair, network_faults::DUPLICATE, f.duplicate_ppm)
        {
            if let Some(duplicate_at) = at.checked_add(1) {
                layer.network_counts.duplicated += 1;
                deliveries.push((duplicate_at, deliveries[0].1.clone()));
            }
        }
        deliveries
    }
    /// Fixed little-endian, length-prefixed encoding, ordered by pair/fault ID.
    pub fn witness(&self, h: &mut Sha256) {
        fn policy(h: &mut Sha256, p: Policy) {
            for n in [p.latency_ns, p.jitter_ns, p.loss_ppm] {
                h.update(n.to_le_bytes());
            }
        }
        policy(h, self.global);
        h.update((self.baselines.len() as u64).to_le_bytes());
        for (&(a, b), &p) in &self.baselines {
            pair_hash(h, (a, b));
            policy(h, p);
        }
        h.update((self.last.len() as u64).to_le_bytes());
        for (&pair, &last) in &self.last {
            pair_hash(h, pair);
            h.update(last.to_le_bytes());
        }
        h.update((self.loss.len() as u64).to_le_bytes());
        for (&pair, loss) in &self.loss {
            pair_hash(h, pair);
            h.update(loss.counter().to_le_bytes());
            h.update(self.jitter[&pair].counter().to_le_bytes());
        }
        h.update((self.layers.len() as u64).to_le_bytes());
        for (&id, layer) in &self.layers {
            h.update(id.to_le_bytes());
            string_hash(h, &layer.handle);
            h.update((layer.pairs.len() as u64).to_le_bytes());
            for &pair in &layer.pairs {
                pair_hash(h, pair);
            }
            policy(h, layer.policy);
        }
        // Append nothing for historical unlimited policies, even with shapes.
        if self.bandwidth_used {
            h.update(b"seedbox-link-bandwidth-v1");
            fn bandwidth(h: &mut Sha256, p: Policy) {
                h.update(p.bandwidth_bps.unwrap_or(0).to_le_bytes());
                h.update([u8::from(p.queue_bound_ns.is_some())]);
                if let Some(bound) = p.queue_bound_ns {
                    h.update(bound.to_le_bytes());
                }
            }
            bandwidth(h, self.global);
            h.update((self.baselines.len() as u64).to_le_bytes());
            for (&pair, &p) in &self.baselines {
                pair_hash(h, pair);
                bandwidth(h, p);
            }
            h.update((self.layers.len() as u64).to_le_bytes());
            for (&id, layer) in &self.layers {
                h.update(id.to_le_bytes());
                bandwidth(h, layer.policy);
                layer.queue_counts.witness(h);
            }
            h.update((self.last_depart.len() as u64).to_le_bytes());
            for (&pair, &depart) in &self.last_depart {
                pair_hash(h, pair);
                h.update(depart.to_le_bytes());
            }
            h.update((self.queue_counts.len() as u64).to_le_bytes());
            for (&pair, &counts) in &self.queue_counts {
                pair_hash(h, pair);
                counts.witness(h);
            }
        }
        if self.network_used {
            h.update(b"seedbox-network-faults-v1");
            self.network_streams.witness(h);
            h.update((self.layers.len() as u64).to_le_bytes());
            for (&id, layer) in &self.layers {
                h.update(id.to_le_bytes());
                let f = layer.network_faults;
                for n in [
                    f.reorder_ppm,
                    f.reorder_delay_ns,
                    f.duplicate_ppm,
                    f.burst_loss_ppm,
                    f.burst_length,
                    f.latency_spike_ppm,
                    f.latency_spike_ns,
                    f.tcp_reset_ppm,
                ] {
                    h.update(n.to_le_bytes());
                }
                layer.network_counts.witness(h);
                h.update((layer.burst_remaining.len() as u64).to_le_bytes());
                for (&pair, &remaining) in &layer.burst_remaining {
                    pair_hash(h, pair);
                    h.update(remaining.to_le_bytes());
                }
            }
        }
    }
}
fn optional_u64(value: &Value, key: &str) -> Result<Option<u64>, String> {
    value
        .get(key)
        .map(|v| v.as_u64().ok_or_else(|| format!("invalid {key}")))
        .transpose()
}
pub fn pair_hash(h: &mut Sha256, (a, b): Pair) {
    h.update((a as u64).to_le_bytes());
    h.update((b as u64).to_le_bytes());
}
pub fn string_hash(h: &mut Sha256, s: &str) {
    h.update((s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}
/// Broadcast candidates retain ascending machine index order; unicast uses its MAC owner.
pub fn candidates(src: usize, mac: [u8; 6], macs: &[[u8; 6]]) -> Vec<usize> {
    if mac == [0xff; 6] {
        (0..macs.len()).filter(|&n| n != src).collect()
    } else {
        macs.iter()
            .position(|m| *m == mac)
            .into_iter()
            .filter(|&n| n != src)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn names() -> Vec<String> {
        ["a", "b", "c"].into_iter().map(str::to_owned).collect()
    }
    fn policy(latency_ns: u64, jitter_ns: u64, loss_ppm: u64) -> Policy {
        Policy {
            latency_ns,
            jitter_ns,
            loss_ppm,
            bandwidth_bps: None,
            queue_bound_ns: None,
        }
    }
    fn links(p: Policy) -> Links {
        Links::new(1009, &names(), p, BTreeMap::new()).unwrap()
    }
    fn draws(l: &Links, p: Pair) -> (u64, u64) {
        (l.loss[&p].counter(), l.jitter[&p].counter())
    }
    fn impair(l: &mut Links, send: u64, frame: &[u8]) -> (Outcome, Vec<(u64, Vec<u8>)>) {
        let outcome = l.decide(0, 1, send, frame.len(), false);
        let deliveries = match outcome {
            Outcome::Deliver(at) => l.deliveries((0, 1), at, frame),
            _ => vec![],
        };
        (outcome, deliveries)
    }
    #[test]
    fn inactive_faults_add_zero_draws_and_active_faults_do_not_move_legacy_streams() {
        let p = policy(100, 50, 500_000);
        let mut plain = links(p);
        let mut off = plain.clone();
        off.shape_with_faults(
            1,
            "s".into(),
            BTreeSet::from([(0, 1)]),
            p,
            NetworkFaults::default(),
        )
        .unwrap();
        plain
            .shape(1, "s".into(), BTreeSet::from([(0, 1)]), p)
            .unwrap();
        for n in 0..128 {
            assert_eq!(
                impair(&mut off, n, &[0; 60]),
                impair(&mut plain, n, &[0; 60])
            );
        }
        assert_eq!(hash(&off), hash(&plain));
        assert_eq!(off.network_streams.counters((0, 1)), [0; 5]);
        let mut on = links(p);
        let f = NetworkFaults {
            reorder_ppm: 500_000,
            reorder_delay_ns: 100,
            duplicate_ppm: 500_000,
            burst_loss_ppm: 500_000,
            burst_length: 3,
            latency_spike_ppm: 500_000,
            latency_spike_ns: 200,
            tcp_reset_ppm: 500_000,
        };
        on.shape_with_faults(1, "s".into(), BTreeSet::from([(0, 1)]), p, f)
            .unwrap();
        let mut reference = links(p);
        for n in 0..128 {
            if n % 7 == 0 {
                on.selected_drop(0, 1);
                reference.selected_drop(0, 1);
            }
            let cut = n % 11 == 0;
            let outcome = on.decide(0, 1, n, 60, cut);
            if let Outcome::Deliver(at) = outcome {
                on.deliveries((0, 1), at, &[0; 60]);
            }
            reference.decide(0, 1, n, 60, cut);
            assert_eq!(draws(&on, (0, 1)), draws(&reference, (0, 1)));
        }
        assert_eq!(on.stream_sample((0, 1)), reference.stream_sample((0, 1)));
        assert_eq!(on.network_streams.counters((0, 2)), [0; 5]);
        assert_eq!(
            on.network_streams.counters((0, 1))[network_faults::RESET],
            0,
            "non-TCP never samples reset"
        );
        let draws = on.network_streams.counters((0, 1));
        assert_eq!(on.decide(0, 1, 200, 60, true), Outcome::Cut);
        on.selected_drop(0, 1);
        assert_eq!(on.network_streams.counters((0, 1)), draws);
        on.heal("s");
        for n in 201..210 {
            impair(&mut on, n, &[0; 60]);
        }
        assert_eq!(
            on.network_streams.counters((0, 1)),
            draws,
            "healing stops new draws"
        );
    }
    #[test]
    fn bursts_have_exact_lengths_and_reserve_no_bandwidth_or_fifo_clocks() {
        let p = rate(None);
        let f = NetworkFaults {
            burst_loss_ppm: 200_000,
            burst_length: 3,
            ..Default::default()
        };
        let mut l = links(p);
        l.shape_with_faults(1, "s".into(), BTreeSet::from([(0, 1)]), p, f)
            .unwrap();
        let mut rng = Rng::new(1009, "link-burst-loss/a/b");
        let mut remaining = 0;
        let mut lost = 0;
        for n in 0..128 {
            let drop = if remaining > 0 {
                remaining -= 1;
                true
            } else if (rng.next() as u128) * 1_000_000 < (f.burst_loss_ppm as u128) << 64 {
                remaining = 2;
                true
            } else {
                false
            };
            let last = l.last.clone();
            let depart = l.last_depart.clone();
            let result = l.decide(0, 1, n, 1500, false);
            assert_eq!(result == Outcome::Lost, drop);
            if drop {
                lost += 1;
                assert_eq!(l.last, last);
                assert_eq!(l.last_depart, depart);
            }
        }
        assert!(lost > 3 && lost < 128);
        assert_eq!(l.layers[&1].network_counts.burst_dropped, lost);
        assert_eq!(
            l.network_streams.counters((0, 1)),
            [0, 0, rng.counter(), 0, 0]
        );
        assert_eq!(l.layers[&1].burst_remaining[&(0, 1)], remaining);
    }
    #[test]
    fn delays_overtake_copies_are_adjacent_and_each_fault_has_its_own_stream() {
        let p = policy(10, 0, 0);
        let f = NetworkFaults {
            reorder_ppm: 1_000_000,
            reorder_delay_ns: 1000,
            duplicate_ppm: 1_000_000,
            latency_spike_ppm: 1_000_000,
            latency_spike_ns: 100,
            ..Default::default()
        };
        let mut l = links(p);
        l.shape_with_faults(1, "s".into(), BTreeSet::from([(0, 1)]), p, f)
            .unwrap();
        let mut rng = Rng::new(1009, "link-reorder/a/b");
        let mut arrivals = vec![];
        for n in 0..32 {
            rng.next();
            let extra = rng.below(1000) + 1;
            let (base, frames) = impair(&mut l, n, &[n as u8; 60]);
            assert_eq!(base, Outcome::Deliver(n + 10));
            assert_eq!(
                frames,
                vec![
                    (n + 110 + extra, vec![n as u8; 60]),
                    (n + 111 + extra, vec![n as u8; 60])
                ]
            );
            arrivals.push(frames[0].0);
        }
        assert!(
            arrivals.windows(2).any(|p| p[1] < p[0]),
            "actually overtakes"
        );
        assert_eq!(l.network_streams.counters((0, 1)), [64, 32, 0, 32, 0]);
        assert_eq!(l.decide(1, 0, 0, 60, false), Outcome::Deliver(10));
        let mut spike_only = links(p);
        spike_only
            .shape_with_faults(
                1,
                "s".into(),
                BTreeSet::from([(0, 1)]),
                p,
                NetworkFaults {
                    latency_spike_ppm: 1_000_000,
                    latency_spike_ns: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            impair(&mut spike_only, 0, &[0; 60]).1,
            vec![(110, vec![0; 60])]
        );
        assert_eq!(spike_only.network_streams.counters((0, 1)), [0, 0, 0, 1, 0]);
    }
    #[test]
    fn layers_heal_in_either_order_and_snapshots_retain_streams_bursts_and_witnesses() {
        let p = policy(10, 0, 0);
        let f = NetworkFaults {
            reorder_ppm: 500_000,
            reorder_delay_ns: 100,
            duplicate_ppm: 500_000,
            burst_loss_ppm: 100_000,
            burst_length: 5,
            ..Default::default()
        };
        for order in [["old", "new"], ["new", "old"]] {
            let mut l = links(p);
            l.shape_with_faults(1, "old".into(), BTreeSet::from([(0, 1)]), p, f)
                .unwrap();
            for n in 0..16 {
                impair(&mut l, n, &[0; 60]);
            }
            let before = l.clone();
            l.shape(2, "new".into(), BTreeSet::from([(0, 1)]), p)
                .unwrap();
            for n in 16..20 {
                impair(&mut l, n, &[0; 60]);
            }
            assert_eq!(
                l.network_streams.counters((0, 1)),
                before.network_streams.counters((0, 1)),
                "masked layer does not draw"
            );
            assert_eq!(
                l.layers[&1].burst_remaining,
                before.layers[&1].burst_remaining
            );
            let saved = l.clone();
            let sequence = |l: &mut Links| {
                l.heal(order[0]);
                let a = impair(l, 20, &[0; 60]);
                l.heal(order[1]);
                (a, impair(l, 21, &[0; 60]), hash(l))
            };
            let observed = sequence(&mut l);
            let mut restored = saved.clone();
            assert_eq!(sequence(&mut restored), observed);
            let mut branched = before.clone();
            branched.reseed_branch(42, Some("branch"), &names());
            assert_eq!(
                branched.layers[&1].burst_remaining,
                before.layers[&1].burst_remaining
            );
            assert_eq!(branched.last, before.last);
            assert_eq!(branched.network_streams.counters((0, 1)), [0; 5]);
            let mut same = before.clone();
            same.reseed_branch(42, Some("branch"), &names());
            for n in 20..64 {
                assert_eq!(
                    impair(&mut branched, n, &[0; 60]),
                    impair(&mut same, n, &[0; 60])
                );
            }
            let mut changed = before.clone();
            changed
                .layers
                .get_mut(&1)
                .unwrap()
                .burst_remaining
                .insert((0, 1), 99);
            assert_ne!(hash(&changed), hash(&before));
            let mut changed = before.clone();
            changed
                .layers
                .get_mut(&1)
                .unwrap()
                .network_counts
                .duplicated += 1;
            assert_ne!(hash(&changed), hash(&before));
            let mut changed = before.clone();
            changed
                .layers
                .get_mut(&1)
                .unwrap()
                .network_faults
                .reorder_ppm += 1;
            assert_ne!(hash(&changed), hash(&before));
        }
    }
    #[test]
    fn reset_only_samples_eligible_tcp_and_healing_retains_scheduled_reset_bytes() {
        let p = policy(10, 0, 0);
        let mut l = links(p);
        l.shape_with_faults(
            1,
            "s".into(),
            BTreeSet::from([(0, 1)]),
            p,
            NetworkFaults {
                tcp_reset_ppm: 1_000_000,
                ..Default::default()
            },
        )
        .unwrap();
        let mut frame = vec![0; 55];
        frame[12..14].copy_from_slice(&[8, 0]);
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&41u16.to_be_bytes());
        frame[22] = 64;
        frame[23] = 6;
        frame[34..38].copy_from_slice(&[0x0f, 0xa0, 0, 80]);
        frame[46] = 0x50;
        frame[47] = 0x18;
        for bytes in [&[0; 60][..], &frame[..54]] {
            impair(&mut l, 0, bytes);
        }
        assert_eq!(l.network_streams.counters((0, 1)), [0; 5]);
        let (base, queued) = impair(&mut l, 1, &frame);
        assert_eq!(base, Outcome::Deliver(12));
        assert_eq!(queued[0].1[47], 0x14);
        assert_eq!(queued[0].1.len(), 54);
        assert_eq!(l.network_streams.counters((0, 1)), [0, 0, 0, 0, 1]);
        assert_eq!(l.layers[&1].network_counts.tcp_reset, 1);
        l.heal("s");
        assert_eq!(queued[0].1[47], 0x14);
        assert_eq!(impair(&mut l, 2, &frame).1[0].1, frame);
        assert_eq!(l.network_streams.counters((0, 1)), [0, 0, 0, 0, 1]);
    }
    #[test]
    fn floor_and_policy_bounds_are_validated_again_in_the_vmm() {
        assert!(policy(9, 0, 0).validate(10).is_err());
        assert!(policy(10, 0, 0).validate(10).is_ok());
        assert!(policy(10, 10, 0).validate(10).is_err());
        assert!(policy(10, 0, 1_000_001).validate(10).is_err());
        assert!(policy(3_600_000_000_000, 1, 0).validate(10).is_err());
        assert!(policy(u64::MAX, 1, 0).validate(10).is_err());
        let bad =
            serde_json::json!({"latency_ns":10,"links":[{"from":"a","to":"b","latency_ns":9}]});
        assert!(Links::from_config(0, &names(), &bad).is_err());
        let good =
            serde_json::json!({"latency_ns":10,"links":[{"from":"a","to":"b","latency_ns":10}]});
        assert!(Links::from_config(0, &names(), &good).is_ok());
    }
    #[test]
    fn delay_decrease_keeps_fifo_and_direction_is_asymmetric() {
        let mut l = links(policy(10, 0, 0));
        l.shape(1, "s".into(), BTreeSet::from([(0, 1)]), policy(100, 0, 0))
            .unwrap();
        assert_eq!(l.decide(0, 1, 10, 1500, false), Outcome::Deliver(110));
        assert_eq!(l.decide(1, 0, 10, 1500, false), Outcome::Deliver(20));
        l.heal("s");
        assert_eq!(l.decide(0, 1, 11, 1500, false), Outcome::Deliver(111));
        assert_eq!(l.decide(0, 1, 12, 1500, false), Outcome::Deliver(112));
    }
    #[test]
    fn loss_endpoints_and_middle_consume_exactly_two_independent_draws() {
        for ppm in [0, 500_000, 1_000_000] {
            let mut l = links(policy(10, 9, ppm));
            let mut loss = Rng::new(1009, "link-loss/a/b");
            let mut jitter = Rng::new(1009, "link/a/b");
            let mut lost = 0;
            let mut last = 0;
            for n in 0..128 {
                let x = loss.next();
                let j = ((jitter.next() as u128 * 10) >> 64) as u64;
                let expected = if (x as u128) * 1_000_000 < (ppm as u128) << 64 {
                    lost += 1;
                    Outcome::Lost
                } else {
                    last = (n + 10 + j).max(last + 1);
                    Outcome::Deliver(last)
                };
                assert_eq!(l.decide(0, 1, n, 1500, false), expected);
                assert_eq!(draws(&l, (0, 1)), (n + 1, n + 1));
                assert_eq!(l.last.get(&(0, 1)).copied().unwrap_or(0), last);
            }
            match ppm {
                0 => assert_eq!(lost, 0),
                1_000_000 => assert_eq!(lost, 128),
                _ => assert_eq!(lost, 69),
            }
        }
    }
    #[test]
    fn cuts_consume_no_draws_and_unrelated_traffic_does_not_change_samples() {
        let mut l = links(policy(10, 9, 500_000));
        let mut reference = l.clone();
        assert_eq!(l.decide(0, 1, 0, 1500, true), Outcome::Cut);
        assert_eq!(draws(&l, (0, 1)), (0, 0));
        for n in 0..32 {
            l.decide(0, 2, n, 1500, false);
            assert_eq!(
                l.decide(0, 1, n, 1500, false),
                reference.decide(0, 1, n, 1500, false)
            );
        }
    }
    #[test]
    fn broadcast_candidates_draw_in_index_order_and_unicast_uses_owner() {
        let macs = [[0, 0, 0, 0, 0, 1], [0, 0, 0, 0, 0, 2], [0, 0, 0, 0, 0, 3]];
        assert_eq!(candidates(1, [0xff; 6], &macs), vec![0, 2]);
        assert_eq!(candidates(1, macs[0], &macs), vec![0]);
        assert!(candidates(1, macs[1], &macs).is_empty());
        assert!(candidates(1, [0; 6], &macs).is_empty());
        let mut l = links(policy(10, 0, 1_000_000));
        for dst in candidates(1, [0xff; 6], &macs) {
            assert_eq!(l.decide(1, dst, 10, 1500, false), Outcome::Lost);
        }
        assert_eq!(draws(&l, (1, 0)), (1, 1));
        assert_eq!(draws(&l, (1, 2)), (1, 1));
        assert_eq!(draws(&l, (0, 2)), (0, 0));
    }
    #[test]
    fn restore_and_reseed_preserve_baselines_fifo_and_stream_positions() {
        let mut l = Links::new(
            1009,
            &names(),
            policy(10, 0, 0),
            BTreeMap::from([((0, 1), policy(20, 9, 500_000))]),
        )
        .unwrap();
        l.decide(0, 1, 10, 1500, false);
        let snapshot = l.clone();
        let expected = l.decide(0, 1, 11, 1500, false);
        l.decide(0, 1, 12, 1500, false);
        l = snapshot;
        assert_eq!(l.decide(0, 1, 11, 1500, false), expected);
        let last = l.last.clone();
        let baselines = l.baselines.clone();
        l.reseed(42, &names());
        assert_eq!(draws(&l, (0, 1)), (0, 0));
        assert_eq!(l.last, last);
        assert_eq!(l.baselines, baselines);
    }
    #[test]
    fn witness_changes_for_draws_fifo_baselines_and_layers() {
        fn hash(l: &Links) -> Vec<u8> {
            let mut h = Sha256::new();
            l.witness(&mut h);
            h.finalize().to_vec()
        }
        let l = links(policy(10, 0, 0));
        let original = hash(&l);
        let mut changed = l.clone();
        changed.decide(0, 1, 1, 1500, false);
        assert_ne!(hash(&changed), original);
        let mut changed = l.clone();
        changed.last.insert((0, 1), 42);
        assert_ne!(hash(&changed), original);
        let mut changed = l.clone();
        changed.baselines.insert((0, 1), policy(20, 0, 0));
        assert_ne!(hash(&changed), original);
        let mut changed = l.clone();
        changed
            .shape(1, "s".into(), BTreeSet::from([(0, 1)]), policy(20, 0, 0))
            .unwrap();
        assert_ne!(hash(&changed), original);
        assert_eq!(hash(&l.clone()), original);
    }
    #[test]
    fn only_baselines_or_an_applied_shape_put_links_in_use() {
        let mut l = links(policy(10, 0, 0));
        assert!(!l.in_use());
        l.decide(0, 1, 1, 1500, false);
        assert!(!l.in_use());
        l.shape(1, "s".into(), BTreeSet::from([(0, 1)]), policy(20, 0, 0))
            .unwrap();
        l.heal("s");
        assert!(l.in_use(), "a healed shape still shaped the run");
        let based = Links::new(
            1,
            &names(),
            policy(10, 0, 0),
            BTreeMap::from([((0, 1), policy(20, 0, 0))]),
        )
        .unwrap();
        assert!(based.in_use());
    }
    fn rate(bound: Option<u64>) -> Policy {
        Policy {
            bandwidth_bps: Some(1_000_000),
            queue_bound_ns: bound,
            ..policy(1_000_000, 0, 0)
        }
    }
    fn hash(l: &Links) -> Vec<u8> {
        let mut h = Sha256::new();
        l.witness(&mut h);
        h.finalize().to_vec()
    }
    #[test]
    fn bandwidth_serializes_actual_bytes_per_direction_with_ceiling_rounding() {
        let mut l = links(policy(1_000_000, 0, 0));
        l.shape(1, "rate".into(), BTreeSet::from([(0, 1)]), rate(None))
            .unwrap();
        assert_eq!(l.decide(0, 1, 0, 1500, false), Outcome::Deliver(13_000_000));
        assert_eq!(l.decide(0, 1, 0, 1500, false), Outcome::Deliver(25_000_000));
        assert_eq!(l.decide(1, 0, 0, 1500, false), Outcome::Deliver(1_000_000));
        let mut fractional = links(Policy {
            bandwidth_bps: Some(3_000_000_000),
            ..rate(None)
        });
        assert_eq!(
            fractional.decide(0, 1, 0, 1, false),
            Outcome::Deliver(1_000_003)
        );
    }
    #[test]
    fn tail_drop_is_strict_and_reserves_neither_departure_nor_fifo_arrival() {
        for bound in [12_000_000, 23_999_999, 24_000_000] {
            let mut l = links(rate(Some(bound)));
            assert_eq!(l.decide(0, 1, 0, 1500, false), Outcome::Deliver(13_000_000));
            let expected = if bound < 24_000_000 {
                Outcome::QueueDropped
            } else {
                Outcome::Deliver(25_000_000)
            };
            assert_eq!(l.decide(0, 1, 0, 1500, false), expected);
            assert_eq!(
                l.last_depart[&(0, 1)],
                if bound < 24_000_000 {
                    12_000_000
                } else {
                    24_000_000
                }
            );
            assert_eq!(
                l.last[&(0, 1)],
                if bound < 24_000_000 {
                    13_000_000
                } else {
                    25_000_000
                }
            );
            assert_eq!(draws(&l, (0, 1)), (2, 2));
            assert_eq!(
                l.queue_counts[&(0, 1)],
                QueueCounts {
                    matched: 2,
                    queue_dropped: u64::from(bound < 24_000_000),
                    delayed_max_ns: 24_000_000
                }
            );
            if bound < 24_000_000 {
                assert_eq!(
                    l.decide(0, 1, 12_000_000, 1500, false),
                    Outcome::Deliver(25_000_000)
                );
            }
        }
        assert_eq!(
            links(rate(Some(0))).decide(0, 1, 0, 1, false),
            Outcome::QueueDropped
        );
    }
    #[test]
    fn bandwidth_never_changes_existing_draw_order_or_cut_and_selected_drop_semantics() {
        let p = Policy {
            loss_ppm: 500_000,
            jitter_ns: 100_000,
            ..rate(Some(12_000_000))
        };
        let mut l = links(p);
        let mut reference = links(Policy {
            bandwidth_bps: None,
            queue_bound_ns: None,
            ..p
        });
        for send in 0..128 {
            if send % 7 == 0 {
                l.selected_drop(0, 1);
                reference.selected_drop(0, 1);
            }
            let actual = l.decide(0, 1, send, 1500, send % 11 == 0);
            let old = reference.decide(0, 1, send, 1500, send % 11 == 0);
            assert_eq!(
                matches!(actual, Outcome::Lost),
                matches!(old, Outcome::Lost)
            );
            assert_eq!(matches!(actual, Outcome::Cut), matches!(old, Outcome::Cut));
            assert_eq!(draws(&l, (0, 1)), draws(&reference, (0, 1)));
        }
        assert_eq!(l.stream_sample((0, 1)), reference.stream_sample((0, 1)));
        let mut excluded = links(rate(None));
        assert_eq!(excluded.decide(0, 1, 0, 1500, true), Outcome::Cut);
        excluded.selected_drop(0, 1);
        assert!(excluded.last_depart.is_empty());
        assert!(excluded.queue_counts.is_empty());
        let mut lost = links(Policy {
            loss_ppm: 1_000_000,
            ..rate(None)
        });
        assert_eq!(lost.decide(0, 1, 0, 1500, false), Outcome::Lost);
        assert!(lost.last_depart.is_empty());
    }
    #[test]
    fn snapshot_reseed_heal_and_rate_changes_preserve_committed_backlog_and_witness_state() {
        let mut l = links(policy(1_000_000, 0, 0));
        l.shape(1, "slow".into(), BTreeSet::from([(0, 1)]), rate(None))
            .unwrap();
        l.decide(0, 1, 0, 1500, false);
        l.shape(
            2,
            "fast".into(),
            BTreeSet::from([(0, 1)]),
            Policy {
                bandwidth_bps: Some(2_000_000),
                ..rate(None)
            },
        )
        .unwrap();
        assert_eq!(l.decide(0, 1, 0, 1500, false), Outcome::Deliver(19_000_000));
        let snapshot = l.clone();
        l.heal("fast");
        assert_eq!(l.decide(0, 1, 0, 1500, false), Outcome::Deliver(31_000_000));
        let expected = hash(&l);
        l = snapshot.clone();
        l.heal("fast");
        l.decide(0, 1, 0, 1500, false);
        assert_eq!(hash(&l), expected);
        let depart = l.last_depart.clone();
        let counts = l.queue_counts.clone();
        l.reseed(42, &names());
        assert_eq!(l.last_depart, depart);
        assert_eq!(l.queue_counts, counts);
        l.heal("slow");
        assert_eq!(l.decide(0, 1, 0, 1500, false), Outcome::Deliver(31_000_001));
        assert_eq!(l.last_depart, depart);
        for variant in 0..4 {
            let mut changed = snapshot.clone();
            match variant {
                0 => changed.layers.get_mut(&2).unwrap().policy.queue_bound_ns = Some(20_000_000),
                1 => {
                    changed.last_depart.insert((0, 1), 19_000_000);
                }
                2 => changed.queue_counts.get_mut(&(0, 1)).unwrap().queue_dropped += 1,
                _ => changed.layers.get_mut(&2).unwrap().queue_counts.matched += 1,
            }
            assert_ne!(hash(&changed), hash(&snapshot));
        }
        assert!(
            links(rate(None)).in_use(),
            "global rate must fold network state into the world witness"
        );
    }
    #[test]
    fn bandwidth_configuration_and_timestamp_overflow_are_bounded() {
        for net in [
            serde_json::json!({"bandwidth_bps":0}),
            serde_json::json!({"bandwidth_bps":1.5}),
            serde_json::json!({"queue_bound_ns":1}),
            serde_json::json!({"bandwidth_bps":1,"queue_bound_ns":-1}),
            serde_json::json!({"links":[{"from":"a","to":"b","latency_ns":1_000_000,"bandwidth_bps":0}]}),
        ] {
            assert!(Links::from_config(1, &names(), &net).is_err(), "{net}");
        }
        assert!(Policy {
            bandwidth_bps: Some(0),
            ..rate(None)
        }
        .validate(1)
        .is_err());
        assert!(Policy {
            bandwidth_bps: None,
            ..rate(Some(1))
        }
        .validate(1)
        .is_err());
        let mut l = links(rate(None));
        assert_eq!(l.decide(0, 1, u64::MAX, 1500, false), Outcome::QueueDropped);
        assert!(l.last_depart.is_empty());
        assert!(l.last.is_empty());
        let mut l = links(Policy {
            bandwidth_bps: Some(1),
            ..rate(None)
        });
        assert_eq!(l.decide(0, 1, 0, usize::MAX, false), Outcome::QueueDropped);
    }
}
