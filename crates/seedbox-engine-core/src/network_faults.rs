//! Evidence values for directed network impairments executed by the Driver.
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Counts {
    pub matched: u64,
    /// Candidates given an extra reorder delay; receipt order is workload evidence.
    pub reordered: u64,
    pub duplicated: u64,
    pub burst_dropped: u64,
    pub spiked: u64,
    pub tcp_reset: u64,
    pub delayed_max_ns: u64,
}
impl Counts {
    pub fn witness(self, h: &mut Sha256) {
        for n in [
            self.matched,
            self.reordered,
            self.duplicated,
            self.burst_dropped,
            self.spiked,
            self.tcp_reset,
            self.delayed_max_ns,
        ] {
            h.update(n.to_le_bytes());
        }
    }
}
