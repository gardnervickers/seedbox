//! Guest checkpoint integration through the same public worker entrypoint as the CLI.
use seedbox_vmm::{run, RunReport, WorkerRequest};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[test]
#[cfg_attr(not(feature = "guest-tests"), ignore = "requires guest-tests")]
fn runtime_checkpoint_getrandom_and_urandom_repeat_and_diverge() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let input = |key: &str, default: &str| -> PathBuf {
        std::env::var_os(key)
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(default))
    };
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("world.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "kernel": input("SEEDBOX_TEST_KERNEL", "artifacts/bzImage"),
            "mem_mb": 256, "duration_ns": 10_000_000_000u64,
            "network": {"latency_ns": 1_000_000},
            "machines": [
                {"name":"a", "initrd":input("SEEDBOX_TEST_INITRD", "artifacts/entropy-fork.cpio")},
                {"name":"b", "initrd":input("SEEDBOX_TEST_INITRD", "artifacts/entropy-fork.cpio")}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let at = 2_000_000_000;
    let mut attempts = Vec::new();
    for attempt in ["first", "second"] {
        let output = directory.path().join(attempt);
        let mut request = WorkerRequest::new(config.clone(), 7, output.clone());
        request.fork_seeds = vec![42, 43];
        request.checkpoint_at_ns = vec![at];
        let RunReport::Checkpoints { checkpoints, .. } = run(&request, None).unwrap() else {
            panic!("expected checkpoint branch reports");
        };
        assert_eq!(checkpoints.len(), 3);
        let mut branches = BTreeMap::new();
        for report in checkpoints {
            let checkpoint = report.checkpoint.unwrap();
            assert_eq!(checkpoint.at_ns, at);
            let expected_reseed = if checkpoint.branch_seed == 7 {
                "continuation"
            } else {
                "time_page"
            };
            assert!(report
                .checkpoint_reseed
                .unwrap()
                .iter()
                .all(|r| r["reseed"] == expected_reseed));
            let mut values = Vec::new();
            for machine in report.machines {
                assert_eq!(machine.outcome.guest_exit_code, Some(0));
                let console = std::fs::read_to_string(output.join(format!(
                    "checkpoint-{at}/seed-{}/{}.console",
                    checkpoint.branch_seed, machine.outcome.name
                )))
                .unwrap();
                assert!(console.contains("before fork:"));
                let line = console
                    .lines()
                    .find(|line| line.starts_with("after fork:"))
                    .unwrap();
                let random: Vec<_> = line.split_whitespace().skip(2).map(str::to_owned).collect();
                assert_eq!(random.len(), 2);
                for (value, source) in random.iter().zip(["getrandom=", "urandom="]) {
                    let hex = value.strip_prefix(source).unwrap();
                    assert_eq!(hex.len(), 32);
                    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
                }
                values.push(random);
            }
            branches.insert(checkpoint.branch_seed, values);
        }
        for machine in 0..2 {
            for source in 0..2 {
                assert_ne!(
                    branches[&42][machine][source],
                    branches[&43][machine][source]
                );
            }
        }
        attempts.push(branches);
    }
    assert_eq!(attempts[0], attempts[1]);
}
