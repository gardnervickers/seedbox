//! Periodic samples of a machine's running exit witness, so offline diff can
//! say which interval of simulated time two runs first diverged in, not only
//! that their final witnesses differ. Diagnostic evidence only: sampling
//! reads the witness without feeding it, so results are unchanged.

use sha2::{Digest, Sha256};

/// Simulated nanoseconds between samples.
pub const INTERVAL_NS: u64 = 1_000_000;

/// Bytes per entry in `NAME.witness-log`: little-endian u64 time_ns,
/// records and the first eight witness bytes read as little-endian.
pub const ENTRY_BYTES: usize = 24;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WitnessLog {
    entries: Vec<[u64; 3]>,
    /// Witness records so far, across incarnations.
    records: u64,
    /// The next sample is taken by the first record at or after this time.
    next_ns: u64,
}

fn prefix(witness: &Sha256) -> u64 {
    let digest = witness.clone().finalize();
    u64::from_le_bytes(digest[..8].try_into().unwrap())
}

impl WitnessLog {
    /// Count a record just hashed into `witness` and sample it if the
    /// machine has reached the next interval boundary.
    pub fn record(&mut self, time_ns: u64, witness: &Sha256) {
        self.records += 1;
        if time_ns >= self.next_ns {
            self.entries.push([time_ns, self.records, prefix(witness)]);
            self.next_ns = (time_ns / INTERVAL_NS + 1) * INTERVAL_NS;
        }
    }

    /// The samples, followed by the final state when it was not sampled.
    pub fn encode(&self, time_ns: u64, witness: &Sha256) -> Vec<u8> {
        let mut entries = self.entries.clone();
        if entries.last().is_none_or(|e| e[1] != self.records) {
            entries.push([time_ns, self.records, prefix(witness)]);
        }
        let mut bytes = Vec::with_capacity(entries.len() * ENTRY_BYTES);
        for value in entries.iter().flatten() {
            bytes.extend(value.to_le_bytes());
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(bytes: &[u8]) -> Vec<[u64; 3]> {
        bytes
            .chunks_exact(ENTRY_BYTES)
            .map(|e| {
                std::array::from_fn(|i| u64::from_le_bytes(e[i * 8..][..8].try_into().unwrap()))
            })
            .collect()
    }

    #[test]
    fn samples_first_record_of_each_interval_and_appends_final_state() {
        let mut log = WitnessLog::default();
        let mut w = Sha256::new();
        for (t, b) in [
            (0, 1),
            (10, 2),
            (1_000_000, 3),
            (1_500_000, 4),
            (5_200_000, 5),
        ] {
            w.update([b as u8]);
            log.record(t, &w);
        }
        // Same interval as the last sample: only the final entry shows it.
        w.update([6]);
        log.record(5_300_000, &w);
        let got = entries(&log.encode(6_000_000, &w));
        let times: Vec<_> = got.iter().map(|e| (e[0], e[1])).collect();
        // One sample per interval reached; idle intervals add nothing.
        assert_eq!(
            times,
            [(0, 1), (1_000_000, 3), (5_200_000, 5), (6_000_000, 6)]
        );
        assert_eq!(got[3][2], prefix(&w));
        assert_ne!(got[2][2], got[3][2]);
    }

    #[test]
    fn final_state_is_not_repeated_when_last_record_was_sampled() {
        let mut log = WitnessLog::default();
        let w = Sha256::new();
        log.record(0, &w);
        assert_eq!(log.encode(0, &w).len(), ENTRY_BYTES);
        assert_eq!(WitnessLog::default().encode(0, &w).len(), ENTRY_BYTES);
    }

    #[test]
    fn sampling_does_not_change_the_witness() {
        let mut w = Sha256::new();
        w.update(b"x");
        let before = w.clone().finalize();
        WitnessLog::default().record(0, &w);
        assert_eq!(w.finalize(), before);
    }
}
