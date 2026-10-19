//! Source-derived live timer oracles. Suppressed/inconclusive coverage and
//! remaining lifecycle/ISA limits are explicit in signal_models/provenance.json.
#[path = "signal_models/runner.rs"]
mod signal_runner;
use signal_runner::{run, Profile};

#[test]
fn original_signal_timer_pure_model() {
    let out = run("model", &[], Profile::Native);
    assert!(
        out.contains("synthetic-model cases=21 negative=42 scope=synthetic"),
        "{out}"
    );
}

#[test]
fn original_signal_storm_state_population() {
    for args in [["1", "4", "1", "8"], ["1", "1", "0", "1"]] {
        for profile in [
            Profile::Native,
            Profile::Plain(&[]),
            Profile::Plain(&["-disable_traces"]),
        ] {
            let out = run("storm", &args, profile);
            assert_eq!(
                out.lines().filter(|s| s.starts_with("round=")).count(),
                40,
                "{out}"
            );
            assert!(
                out.contains("storm-state rounds=40 queued=960 delivered=960 duplicate=0 lost=0"),
                "{out}"
            );
            assert!(out.contains("time_oracle=LIVE"), "{out}");
            let oracles: Vec<_> = out
                .lines()
                .filter(|line| line.starts_with("storm-oracle round="))
                .collect();
            assert_eq!(oracles.len(), 40, "{out}");
            for line in oracles {
                assert!(line.ends_with("failures=0"), "{line}");
            }
        }
    }
}

fn check_timer_population(out: &str, seed: &str) {
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("signal_models/population.json")).unwrap();
    let key = if seed == "1" {
        "default_cases"
    } else {
        "additional_prof_seed_cases"
    };
    let cases = golden[key].as_array().unwrap();
    for (case_index, case) in cases.iter().enumerate() {
        let ops = case["operations"].as_array().unwrap();
        let header = format!(
            "case seed={} which={} nops={}",
            case["seed"],
            case["which"],
            ops.len()
        );
        assert!(
            out.lines().any(|line| line == header),
            "missing {header}\n{out}"
        );
        for op in ops {
            let expected = format!(
                "op case={case_index} index={} kind={} other={} value={} interval={} alarm={} wait={} old=",
                op["index"],
                op["kind"],
                op["other_thread"],
                op["value_us"],
                op["interval_us"],
                op["alarm_sec"],
                op["wait_us"]
            );
            assert!(
                out.lines().any(|line| line.starts_with(&expected)),
                "missing {expected}\n{out}"
            );
        }
        let wait = if case["which"] == 0 { 50000 } else { 100000 };
        let final_op = format!(
            "op case={case_index} index={} kind=0 other=0 value=0 interval=0 alarm=0 wait={wait} old=",
            ops.len()
        );
        assert!(
            out.lines().any(|line| line.starts_with(&final_op)),
            "missing {final_op}\n{out}"
        );
    }
    assert_eq!(
        out.lines().filter(|s| s.starts_with("op case=")).count(),
        42,
        "{out}"
    );
    assert_eq!(
        out.lines().filter(|s| s.starts_with("case seed=")).count(),
        7,
        "{out}"
    );
    assert!(out.contains("itimer-state cases=7 operations=42"), "{out}");
    assert!(out.contains("wrong_thread=0"), "{out}");
    assert!(out.contains("time_oracle=LIVE"), "{out}");
}

fn unstalled_cases(out: &str) -> std::collections::BTreeSet<usize> {
    let records: Vec<_> = out
        .lines()
        .filter(|line| line.starts_with("timer-oracle case="))
        .collect();
    assert_eq!(records.len(), 7, "{out}");
    records
        .iter()
        .filter_map(|line| {
            let fields: std::collections::BTreeMap<_, _> = line
                .split_whitespace()
                .filter_map(|field| field.split_once('='))
                .collect();
            assert_eq!(fields["failures"], "0", "{line}");
            assert!(
                fields["stalled"] == "0" || fields["stalled"] == "1",
                "{line}"
            );
            // Suppressed checks are reported, never promoted to exact accuracy.
            eprintln!("{line}");
            (fields["stalled"] == "0").then(|| fields["case"].parse().unwrap())
        })
        .collect()
}

#[test]
fn original_signal_itimer_live_profiles() {
    // Native observations are paired with every actual DR profile, as in the
    // original run_itimer. Seed1 keeps all3 variants; the existing extra seed8
    // population exercises appPROF in default and multiplexed release profiles.
    for seed in ["1", "8"] {
        let native = run("itimer", &[seed, "0", "native"], Profile::Native);
        check_timer_population(&native, seed);
        let native_cases = unstalled_cases(&native);
        let mut profiles = vec![
            (Profile::Plain(&[]), "0", "default"),
            (Profile::Timers, "1", "dr-itimers"),
        ];
        if seed == "1" {
            profiles.push((Profile::DebugProf, "1", "prof-pcs"));
        }
        let variants = profiles.len();
        let mut compared = 0;
        for (profile, helper, name) in profiles {
            let out = run("itimer", &[seed, helper, name], profile);
            check_timer_population(&out, seed);
            let dr_cases = unstalled_cases(&out);
            let paired = native_cases.intersection(&dr_cases).count();
            eprintln!(
                "timer-paired seed={seed} profile={name} compared={paired} inconclusive={}",
                7 - paired
            );
            compared += paired;
        }
        // Exact original integer floor, including native/DR pairwise noise.
        assert!(
            compared >= 7 * variants / 4,
            "seed={seed}: compared={compared}, required={}",
            7 * variants / 4
        );
    }
}
