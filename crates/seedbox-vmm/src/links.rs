//! Directed, send-time link decisions. No host, device or scheduler dependencies.
use crate::rng::Rng;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub type Pair = (usize, usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
}
impl Policy {
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
        Ok(())
    }
}
#[derive(Clone)]
pub struct Layer {
    pub handle: String,
    pub pairs: BTreeSet<Pair>,
    pub policy: Policy,
}
#[derive(Clone)]
pub struct Links {
    pub global: Policy,
    pub baselines: BTreeMap<Pair, Policy>,
    pub layers: BTreeMap<u32, Layer>,
    pub last: BTreeMap<Pair, u64>,
    /// A shape layer has been applied at some point in this run.
    pub shaped: bool,
    loss: BTreeMap<Pair, Rng>,
    jitter: BTreeMap<Pair, Rng>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Cut,
    Lost,
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
        for (&(a, b), policy) in &baselines {
            if a >= names.len() || b >= names.len() {
                return Err("invalid baseline pair".into());
            }
            policy.validate(global.latency_ns)?;
        }
        let mut links = Self {
            global,
            baselines,
            layers: BTreeMap::new(),
            last: BTreeMap::new(),
            shaped: false,
            loss: BTreeMap::new(),
            jitter: BTreeMap::new(),
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
        self.loss.clear();
        self.jitter.clear();
        for a in 0..names.len() {
            for b in 0..names.len() {
                if a != b {
                    self.loss.insert(
                        (a, b),
                        Rng::branch(seed, identity, &format!("link-loss/{}/{}", names[a], names[b])),
                    );
                    self.jitter.insert(
                        (a, b),
                        Rng::branch(seed, identity, &format!("link/{}/{}", names[a], names[b])),
                    );
                }
            }
        }
    }
    #[cfg(test)]
    pub fn stream_sample(&mut self, pair: Pair) -> (u64,u64) {
        (self.loss.get_mut(&pair).unwrap().next(),self.jitter.get_mut(&pair).unwrap().next())
    }
    /// The run has baseline overrides or has applied a shape layer.
    pub fn in_use(&self) -> bool {
        self.shaped || !self.baselines.is_empty()
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
        policy.validate(self.global.latency_ns)?;
        if self.layers.contains_key(&id) || self.layers.values().any(|l| l.handle == handle) {
            return Err("duplicate active shape".into());
        }
        self.shaped = true;
        self.layers.insert(
            id,
            Layer {
                handle,
                pairs,
                policy,
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
    pub fn decide(&mut self, src: usize, dst: usize, send: u64, cut: bool) -> Outcome {
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
        let arrival =
            (send + policy.latency_ns + jitter).max(self.last.get(&pair).copied().unwrap_or(0) + 1);
        self.last.insert(pair, arrival);
        Outcome::Deliver(arrival)
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
    }
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
        }
    }
    fn links(p: Policy) -> Links {
        Links::new(1009, &names(), p, BTreeMap::new()).unwrap()
    }
    fn draws(l: &Links, p: Pair) -> (u64, u64) {
        (l.loss[&p].counter(), l.jitter[&p].counter())
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
        assert_eq!(l.decide(0, 1, 10, false), Outcome::Deliver(110));
        assert_eq!(l.decide(1, 0, 10, false), Outcome::Deliver(20));
        l.heal("s");
        assert_eq!(l.decide(0, 1, 11, false), Outcome::Deliver(111));
        assert_eq!(l.decide(0, 1, 12, false), Outcome::Deliver(112));
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
                assert_eq!(l.decide(0, 1, n, false), expected);
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
        assert_eq!(l.decide(0, 1, 0, true), Outcome::Cut);
        assert_eq!(draws(&l, (0, 1)), (0, 0));
        for n in 0..32 {
            l.decide(0, 2, n, false);
            assert_eq!(l.decide(0, 1, n, false), reference.decide(0, 1, n, false));
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
            assert_eq!(l.decide(1, dst, 10, false), Outcome::Lost);
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
        l.decide(0, 1, 10, false);
        let snapshot = l.clone();
        let expected = l.decide(0, 1, 11, false);
        l.decide(0, 1, 12, false);
        l = snapshot;
        assert_eq!(l.decide(0, 1, 11, false), expected);
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
        changed.decide(0, 1, 1, false);
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
        l.decide(0, 1, 1, false);
        assert!(!l.in_use());
        l.shape(1, "s".into(), BTreeSet::from([(0, 1)]), policy(20, 0, 0))
            .unwrap();
        l.heal("s");
        assert!(l.in_use(), "a healed shape still shaped the run");
        let based = Links::new(1, &names(), policy(10, 0, 0), BTreeMap::from([((0, 1), policy(20, 0, 0))])).unwrap();
        assert!(based.in_use());
    }
}
