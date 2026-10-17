//! Counterfactual analysis of one failed simulation: how often its failure
//! recurs when part of it changes.
//!
//! Every arm reruns the source's full program from a fresh start under
//! derived seeds. The baseline varies every draw. A time arm keeps every
//! draw before its time and varies every draw from then on, by adding an
//! anchor and a reseed there and deriving the seeds of the reseeds that
//! follow. A fault arm removes one external fault (with its teardown and the
//! heals of its handle) and varies every draw; its exact run keeps the
//! source's seeds, so it is the source minus that fault and nothing else.
//!
//! Variant `v` derives the same seeds in every arm, so arms differ only in
//! what they change. Rates are of this program's runs under derived seeds,
//! never proof of a cause.
use crate::shrink;
use seedbox_engine_protocol::input_program::{BranchAction, InputAction, TimedInput};
use std::collections::BTreeSet;

/// The key prefix of the inputs a time arm adds.
pub const KEY_PREFIX: &str = "analysis-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arm {
    Baseline,
    ReseedFrom { at_ns: u64 },
    Without { keys: Vec<String> },
}

/// Seed `variant` derived from `base`: `base` itself for 0 (splitmix64).
pub fn derived_seed(base: u64, variant: u32) -> u64 {
    if variant == 0 {
        return base;
    }
    let mut z = base ^ u64::from(variant).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `count` times evenly inside (0, `end_ns`), skipping any that already
/// hold an anchor, so a reseed can be added there.
pub fn times(program: &[TimedInput], end_ns: u64, count: u32) -> Vec<u64> {
    let anchors: BTreeSet<u64> = program
        .iter()
        .filter(|i| matches!(i.action, InputAction::Branch(BranchAction::Anchor {})))
        .map(|i| i.at_ns)
        .collect();
    (1..=u64::from(count))
        .map(|k| (u128::from(end_ns) * u128::from(k) / (u128::from(count) + 1)) as u64)
        .map(|t| t / 1_000_000 * 1_000_000)
        .filter(|t| *t > 0 && !anchors.contains(t))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Each external fault due before `end_ns` that the program could do
/// without: its keys, removed together. Heals go with the fault whose handle
/// they heal, and a generated fault or mutation with its teardown; branch
/// inputs and rules are not faults. A fault due later never applied.
pub fn faults(program: &[TimedInput], end_ns: u64) -> Vec<Vec<String>> {
    let candidates = shrink::candidates(program, 0);
    candidates
        .atoms
        .iter()
        .filter(|atom| !candidates.requires.contains_key(&atom[0]))
        .filter(|atom| {
            program
                .iter()
                .find(|i| i.key == atom[0])
                .is_some_and(|i| matches!(i.action, InputAction::Fault(_)) && i.at_ns < end_ns)
        })
        .map(|atom| {
            let mut keys = atom.clone();
            keys.extend(
                candidates
                    .requires
                    .iter()
                    .filter(|(_, owner)| *owner == &atom[0])
                    .map(|(key, _)| key.clone()),
            );
            keys
        })
        .collect()
}

/// One run of an arm: its program and run seed. Variant 0 keeps the
/// source's seeds (meaningful for a fault arm's exact run); others derive
/// them.
pub fn run(
    program: &[TimedInput],
    seed: u64,
    arm: &Arm,
    variant: u32,
) -> Result<(Vec<TimedInput>, u64), String> {
    let reseed_all = |inputs: &mut [TimedInput], from_ns: u64| {
        for input in inputs.iter_mut().filter(|i| i.at_ns >= from_ns) {
            if let InputAction::Branch(BranchAction::Reseed { seed, .. }) = &mut input.action {
                *seed = derived_seed(*seed, variant);
            }
        }
    };
    match arm {
        Arm::Baseline => {
            let mut inputs = program.to_vec();
            reseed_all(&mut inputs, 0);
            Ok((inputs, derived_seed(seed, variant)))
        }
        Arm::Without { keys } => {
            let mut inputs: Vec<TimedInput> = program
                .iter()
                .filter(|i| !keys.contains(&i.key))
                .cloned()
                .collect();
            reseed_all(&mut inputs, 0);
            Ok((inputs, derived_seed(seed, variant)))
        }
        Arm::ReseedFrom { at_ns } => {
            let at_ns = *at_ns;
            if program.iter().any(|i| {
                i.at_ns == at_ns && matches!(i.action, InputAction::Branch(BranchAction::Anchor {}))
            }) {
                return Err(format!("an anchor already holds {at_ns} ns"));
            }
            let mut inputs = program.to_vec();
            reseed_all(&mut inputs, at_ns);
            let timeout_ns = seedbox_engine_protocol::input_program::default_timeout_ns();
            let at = inputs.partition_point(|i| i.at_ns < at_ns);
            let branch = [
                TimedInput {
                    key: format!("{KEY_PREFIX}anchor"),
                    at_ns,
                    action: InputAction::Branch(BranchAction::Anchor {}),
                    timeout_ns,
                },
                TimedInput {
                    key: format!("{KEY_PREFIX}reseed"),
                    at_ns,
                    action: InputAction::Branch(BranchAction::Reseed {
                        seed: derived_seed(seed ^ at_ns, variant),
                        identity: format!("{KEY_PREFIX}{at_ns}"),
                    }),
                    timeout_ns,
                },
            ];
            inputs.splice(at..at, branch);
            Ok((inputs, seed))
        }
    }
}

/// The 95% Wilson score interval of `failed` of `runs`, in per mille.
pub fn interval(failed: u32, runs: u32) -> Option<[u32; 2]> {
    if runs == 0 {
        return None;
    }
    let (n, p, z) = (
        f64::from(runs),
        f64::from(failed) / f64::from(runs),
        1.96_f64,
    );
    let centre = p + z * z / (2.0 * n);
    let spread = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt();
    let scale = 1.0 + z * z / n;
    let bound = |x: f64| ((x / scale).clamp(0.0, 1.0) * 1000.0).round() as u32;
    Some([bound(centre - spread), bound(centre + spread)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_protocol::input_program::{validate_program, GENERATED_END_SUFFIX};

    fn fault(key: &str, at_ns: u64, json: serde_json::Value) -> TimedInput {
        TimedInput {
            key: key.into(),
            at_ns,
            action: InputAction::Fault(serde_json::from_value(json).unwrap()),
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
        }
    }

    fn program() -> Vec<TimedInput> {
        vec![
            fault(
                "gen-0",
                1_000_000_000,
                serde_json::json!({"action": "pause", "machine": "a"}),
            ),
            fault(
                &format!("gen-0{GENERATED_END_SUFFIX}"),
                2_000_000_000,
                serde_json::json!({"action": "resume", "machine": "a"}),
            ),
            fault(
                "cut",
                3_000_000_000,
                serde_json::json!({"action": "partition", "handle": "h", "left": ["a"], "right": ["b"]}),
            ),
            fault(
                "mend",
                4_000_000_000,
                serde_json::json!({"action": "heal", "handle": "h"}),
            ),
        ]
    }

    #[test]
    fn faults_take_their_teardowns_and_heals() {
        // The partition is due after the run ended: it never applied.
        assert_eq!(faults(&program(), 3_000_000_000).len(), 1);
        assert_eq!(
            faults(&program(), 10_000_000_000),
            vec![
                vec!["gen-0".to_string(), format!("gen-0{GENERATED_END_SUFFIX}")],
                vec!["cut".to_string(), "mend".to_string()],
            ]
        );
    }

    #[test]
    fn a_time_arm_keeps_its_prefix_and_reseeds_after_it() {
        let source = program();
        let (inputs, seed) = run(
            &source,
            7,
            &Arm::ReseedFrom {
                at_ns: 3_000_000_000,
            },
            1,
        )
        .unwrap();
        assert_eq!(seed, 7);
        let keys: Vec<&str> = inputs.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "gen-0",
                "gen-0-end",
                "analysis-anchor",
                "analysis-reseed",
                "cut",
                "mend"
            ]
        );
        validate_program(&inputs, 10_000_000_000).unwrap();
        // Variants differ; the same variant derives the same program.
        let (other, _) = run(
            &source,
            7,
            &Arm::ReseedFrom {
                at_ns: 3_000_000_000,
            },
            2,
        )
        .unwrap();
        assert_ne!(inputs, other);
        assert_eq!(
            inputs,
            run(
                &source,
                7,
                &Arm::ReseedFrom {
                    at_ns: 3_000_000_000
                },
                1
            )
            .unwrap()
            .0
        );
    }

    #[test]
    fn a_fault_arm_exact_run_is_the_source_less_that_fault() {
        let source = program();
        let arm = Arm::Without {
            keys: vec!["cut".into(), "mend".into()],
        };
        let (inputs, seed) = run(&source, 7, &arm, 0).unwrap();
        assert_eq!((inputs, seed), (source[..2].to_vec(), 7));
        assert_ne!(run(&source, 7, &arm, 1).unwrap().1, 7);
    }

    #[test]
    fn times_avoid_existing_anchors_and_round_to_milliseconds() {
        assert_eq!(
            times(&program(), 10_000_000_000, 4),
            [2_000_000_000, 4_000_000_000, 6_000_000_000, 8_000_000_000]
        );
        let mut anchored = program();
        anchored.insert(
            3,
            TimedInput {
                key: "a0".into(),
                at_ns: 4_000_000_000,
                action: InputAction::Branch(BranchAction::Anchor {}),
                timeout_ns: 1,
            },
        );
        assert_eq!(times(&anchored, 10_000_000_000, 4).len(), 3);
    }

    #[test]
    fn wilson_intervals_cover_the_rate() {
        assert_eq!(interval(0, 0), None);
        let [lo, hi] = interval(0, 8).unwrap();
        assert_eq!(lo, 0);
        assert!((300..=330).contains(&hi), "{hi}");
        let [lo, hi] = interval(8, 8).unwrap();
        assert!((670..=700).contains(&lo), "{lo}");
        assert_eq!(hi, 1000);
    }
}
