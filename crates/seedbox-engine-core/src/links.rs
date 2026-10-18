//! Canonical link policy values shared by the executing Driver and public validation.
use seedbox_control::{NetworkFaults, MAX_LINK_DELAY_NS, MAX_PPM};
use sha2::{Digest, Sha256};

pub type Pair = (usize, usize);
/// Global link latency when the network config names none: one millisecond.
pub use seedbox_engine_protocol::world_config::DEFAULT_LATENCY_NS;
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
            .is_none_or(|n| n > MAX_LINK_DELAY_NS)
        {
            return Err("latency_ns + jitter_ns exceeds one hour".into());
        }
        if self.loss_ppm > MAX_PPM {
            return Err(format!("loss_ppm exceeds {MAX_PPM}"));
        }
        self.validate_bandwidth()
    }
    /// A shape layer's policy and impairments, checked against the global floor.
    pub fn validate_shape(self, floor: u64, network_faults: &NetworkFaults) -> Result<(), String> {
        self.validate(floor)?;
        network_faults.validate(self.latency_ns, self.jitter_ns)
    }
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueCounts {
    pub matched: u64,
    pub queue_dropped: u64,
    pub delayed_max_ns: u64,
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
    fn policy() -> Policy {
        Policy {
            latency_ns: 100,
            jitter_ns: 20,
            loss_ppm: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
        }
    }
    #[test]
    fn policy_validates_latency_loss_and_bandwidth_without_an_executor() {
        let base = policy();
        base.validate(100).unwrap();
        for invalid in [
            Policy {
                latency_ns: 99,
                ..base
            },
            Policy {
                jitter_ns: 100,
                ..base
            },
            Policy {
                latency_ns: MAX_LINK_DELAY_NS,
                ..base
            },
            Policy {
                loss_ppm: MAX_PPM + 1,
                ..base
            },
            Policy {
                bandwidth_bps: Some(0),
                ..base
            },
            Policy {
                queue_bound_ns: Some(100),
                ..base
            },
        ] {
            assert!(invalid.validate(100).is_err(), "{invalid:?}");
        }
        Policy {
            bandwidth_bps: Some(1),
            queue_bound_ns: Some(100),
            ..base
        }
        .validate(100)
        .unwrap();
    }
    #[test]
    fn shape_uses_the_public_impairment_bounds() {
        let base = policy();
        base.validate_shape(100, &NetworkFaults::default()).unwrap();
        assert!(base.validate_shape(101, &NetworkFaults::default()).is_err());
        assert!(base
            .validate_shape(
                100,
                &NetworkFaults {
                    duplicate_ppm: MAX_PPM + 1,
                    ..Default::default()
                }
            )
            .is_err());
        assert!(base
            .validate_shape(
                100,
                &NetworkFaults {
                    reorder_ppm: 1,
                    reorder_delay_ns: 0,
                    ..Default::default()
                }
            )
            .is_err());
    }
    #[test]
    fn broadcast_and_unicast_candidates_are_ordered_and_exclude_source() {
        let macs = [[1; 6], [2; 6], [3; 6]];
        assert_eq!(candidates(1, [0xff; 6], &macs), [0, 2]);
        assert_eq!(candidates(1, [3; 6], &macs), [2]);
        assert!(candidates(1, [2; 6], &macs).is_empty());
        assert!(candidates(1, [4; 6], &macs).is_empty());
    }
}
