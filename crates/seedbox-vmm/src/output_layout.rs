//! Observation paths only; never part of world identity or witnesses.

/// A single retained branch uses the ordinary machine-evidence directory.
/// Engine batches keep each (T, B) separate beneath that directory.
pub fn checkpoint_directory(out: &str, times: usize, seeds: usize, at: u64, seed: u64) -> String {
    if times == 1 && seeds == 1 {out.to_string()}
    else {format!("{out}/checkpoint-{at}/seed-{seed}")}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_branches_use_normal_layout_and_batches_stay_separate() {
        assert_eq!(checkpoint_directory("output/machines", 1, 1, 20, 11), "output/machines");
        for (times, seeds) in [(1, 2), (2, 1), (2, 2)] {
            assert_eq!(checkpoint_directory("output/machines", times, seeds, 20, 11), "output/machines/checkpoint-20/seed-11");
        }
    }
}
