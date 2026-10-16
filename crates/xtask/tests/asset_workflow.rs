//! Host-side contract of the production CI asset planner. Python is the
//! implementation language of the workflow step, not a second test runner.
use serde_json::json;
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> String {
    let output = seedbox_test::process::output(
        Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@invalid",
            ])
            .args(args),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn pull_request_compares_base_even_when_previous_head_is_unavailable() {
    let workflow = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/kernels.yml"),
    )
    .unwrap();
    let script = workflow
        .split_once("python3 - <<'PYTHON'\n")
        .expect("production planner Python step")
        .1
        .split_once("          PYTHON")
        .expect("planner heredoc end")
        .0;
    let script = script
        .lines()
        .map(|line| {
            line.strip_prefix("          ")
                .expect("planner indentation")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    for (manifest, source) in [
        ("kernel/inputs.json", "kernel/source"),
        ("tools/runtime/inputs.json", "tools/runtime/source"),
    ] {
        fs::create_dir_all(root.join(manifest).parent().unwrap()).unwrap();
        fs::write(root.join(manifest), json!({"common":[source]}).to_string()).unwrap();
        fs::write(root.join(source), "initial").unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    fs::write(root.join("tools/runtime/source"), "changed").unwrap();
    git(root, &["commit", "-qam", "runtime change"]);
    for (name, event, kernel, runtime) in [
        (
            "pull_request",
            json!({"before":"f".repeat(40),"pull_request":{"base":{"sha":base}}}),
            false,
            true,
        ),
        ("push", json!({"before":base}), false, true),
        ("workflow_dispatch", json!({}), true, true),
    ] {
        fs::write(root.join("event.json"), event.to_string()).unwrap();
        fs::write(root.join("output"), "").unwrap();
        let result = seedbox_test::process::output(
            Command::new("python3")
                .args(["-c", &script])
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GITHUB_EVENT_PATH", root.join("event.json"))
                .env("GITHUB_EVENT_NAME", name)
                .env("GITHUB_OUTPUT", root.join("output"))
                .env("ASSET_RUN_ID", "123"),
        )
        .unwrap();
        assert!(
            result.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read_to_string(root.join("output")).unwrap(), format!("kernel=kernel-123\nkernel_changed={kernel}\nruntime=runtime-123\nruntime_changed={runtime}\n"), "{name}");
    }
}
