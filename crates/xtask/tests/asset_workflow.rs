mod support;
use serde_json::json;
use std::{fs, process::Command};

#[test]
fn workflow_uses_pr_base_after_force_push_and_builds_both_assets_on_dispatch() {
    let workflow =
        fs::read_to_string(support::root().join(".github/workflows/kernels.yml")).unwrap();
    let script = workflow
        .split_once("python3 - <<'PYTHON'\n")
        .unwrap()
        .1
        .split_once("          PYTHON")
        .unwrap()
        .0;
    let indent = script
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap();
    let script = script
        .lines()
        .map(|line| {
            if line.len() >= indent {
                &line[indent..]
            } else {
                ""
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // This is a private, unrelated fixture repository, with no remote or credentials.
    let git = |args: &[&str]| {
        support::success(
            Command::new("git")
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args),
        )
    };
    git(&["init", "-q"]);
    for (manifest, source) in [
        ("kernel/inputs.json", "kernel/source"),
        ("tools/runtime/inputs.json", "tools/runtime/source"),
    ] {
        let path = root.join(manifest);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, json!({"common": [source]}).to_string()).unwrap();
        fs::write(root.join(source), "initial").unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-qm", "initial"]);
    let base = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    fs::write(root.join("tools/runtime/source"), "changed").unwrap();
    git(&["commit", "-qam", "runtime change"]);
    for (name, event, kernel, runtime) in [
        (
            "pull_request",
            json!({"before":"f".repeat(40), "pull_request":{"base":{"sha":base}}}),
            false,
            true,
        ),
        ("push", json!({"before":base}), false, true),
        ("workflow_dispatch", json!({}), true, true),
    ] {
        let event_path = root.join("event.json");
        let output = root.join("output");
        fs::write(&event_path, event.to_string()).unwrap();
        fs::write(&output, "").unwrap();
        support::success(
            Command::new("python3")
                .args(["-c", &script])
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GITHUB_EVENT_PATH", event_path)
                .env("GITHUB_EVENT_NAME", name)
                .env("GITHUB_OUTPUT", &output)
                .env("ASSET_RUN_ID", "123"),
        );
        let fields = fs::read_to_string(output).unwrap();
        assert!(
            fields.contains(&format!("kernel_changed={kernel}\n")),
            "{fields}"
        );
        assert!(
            fields.contains(&format!("runtime_changed={runtime}\n")),
            "{fields}"
        );
    }
}
