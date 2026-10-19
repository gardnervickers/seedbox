//! Partial source-derived state oracles, deliberately not full storm/itimer
//! equivalence. Clock-oracle gaps and option axes are in signal_models/provenance.json.
#[path = "signal_models/runner.rs"]
mod signal_runner;
use signal_runner::{Profile, run};

#[test]
fn original_signal_timer_pure_model() {
    let out = run("model", &[], Profile::Native);
    assert!(
        out.contains("synthetic-model cases=21 negative=42 live_time_oracle=PENDING"),
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
            assert!(out.contains("time_oracle=PENDING"), "{out}");
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
    assert!(out.contains("time_oracle=PENDING"), "{out}");
}

#[test]
fn original_signal_itimer_state_population() {
    // Seed 1 is the unmodified original seven-case default. Seed 8 adds PROF
    // app timers: the original default happens to select REAL and VIRTUAL only.
    for seed in ["1", "8"] {
        for profile in [Profile::Native, Profile::Plain(&[])] {
            check_timer_population(&run("itimer", &[seed, "0"], profile), seed);
        }
    }
}

#[test]
fn original_signal_itimer_multiplexed_state_population() {
    for seed in ["1", "8"] {
        check_timer_population(&run("itimer", &[seed, "1"], Profile::Timers), seed);
    }
}

#[test]
fn original_signal_itimer_prof_pcs_state_population() {
    // A missing actual debug fixture is a setup failure, never a release
    // fallback or skip. Native ARM remains a required separate CI execution.
    let out = run("itimer", &["1", "1"], Profile::DebugProf);
    check_timer_population(&out, "1");
}
