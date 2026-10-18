//! Compile-only cache regression: two real Git worktrees share one private
//! Cargo target. No guest, runtime identity, compiler wrapper or cache reset.
use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, UNIX_EPOCH},
};

fn succeeds(command: &mut Command) -> std::process::Output {
    let output = seedbox_exec::output(command).expect("fixture command starts");
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(root: &Path, arguments: &[&str]) -> std::process::Output {
    succeeds(
        Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Cache fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Cache fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .args(arguments),
    )
}

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn workspace_target_reuse_rebuilds_spec_metadata_in_both_layout_directions() {
    let root = tempfile::tempdir().unwrap();
    let newer = root.path().join("newer");
    let older = root.path().join("older");
    fs::create_dir(&newer).unwrap();
    git(&newer, &["init", "--quiet"]);
    write(&newer, "Cargo.toml", "[workspace]\nmembers=[\"crates/seedbox-spec\",\"crates/seedbox-control\",\"crates/seedbox-api\"]\nresolver=\"2\"\n");
    write(
        &newer,
        ".cargo/config.toml",
        "[env]\nSEEDBOX_BUILD_ROOT={value=\".\",relative=true,force=true}\n",
    );
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let manifest = fs::read_to_string(source_root.join("seedbox-spec/Cargo.toml")).unwrap();
    // Consume the real package's watcher choice. Before the fix this is empty,
    // reproducing stale spec metadata despite the control watcher rebuilding API.
    let spec_build = manifest
        .lines()
        .find(|line| line.starts_with("build = "))
        .unwrap_or("");
    write(
        &newer,
        "crates/seedbox-engine-core/build.rs",
        &fs::read_to_string(source_root.join("seedbox-engine-core/build.rs")).unwrap(),
    );
    write(
        &newer,
        "crates/seedbox-spec/Cargo.toml",
        &format!(
            "[package]\nname=\"seedbox-spec\"\nversion=\"0.0.0\"\nedition=\"2021\"\n{spec_build}\n"
        ),
    );
    write(&newer, "crates/seedbox-control/Cargo.toml", "[package]\nname=\"seedbox-control\"\nversion=\"0.0.0\"\nedition=\"2021\"\nbuild=\"../seedbox-engine-core/build.rs\"\n");
    write(
        &newer,
        "crates/seedbox-control/src/lib.rs",
        "pub fn ready() {}\n",
    );
    write(&newer, "crates/seedbox-api/Cargo.toml", "[package]\nname=\"seedbox-api\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[dependencies]\nseedbox-spec={path=\"../seedbox-spec\"}\nseedbox-control={path=\"../seedbox-control\"}\n");
    write(
        &newer,
        "crates/seedbox-spec/src/lib.rs",
        "pub struct SimulatorDefaults { pub blocks: u32 }\n",
    );
    write(&newer, "crates/seedbox-api/src/lib.rs", "pub fn make() -> seedbox_spec::SimulatorDefaults { seedbox_control::ready(); seedbox_spec::SimulatorDefaults { blocks: 32 } }\n");
    git(&newer, &["add", "."]);
    git(&newer, &["commit", "--quiet", "-m", "older layout"]);
    git(
        &newer,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            older.to_str().unwrap(),
            "HEAD",
        ],
    );
    write(
        &newer,
        "crates/seedbox-spec/src/lib.rs",
        "pub struct SimulatorDefaults { pub blocks: u32, pub backend: u8 }\n",
    );
    write(&newer, "crates/seedbox-api/src/lib.rs", "pub fn make() -> seedbox_spec::SimulatorDefaults { seedbox_control::ready(); seedbox_spec::SimulatorDefaults { blocks: 32, backend: 1 } }\n");
    git(&newer, &["add", "."]);
    git(&newer, &["commit", "--quiet", "-m", "newer layout"]);
    // Caller-chosen historical source mtimes: no sleep or timestamp-resolution
    // assumption is needed to exercise newer-to-older checkout reuse.
    for checkout in [&newer, &older] {
        for relative in [
            "crates/seedbox-spec/src/lib.rs",
            "crates/seedbox-api/src/lib.rs",
        ] {
            fs::File::options()
                .write(true)
                .open(checkout.join(relative))
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(86_400))
                .unwrap();
        }
    }
    let target = root.path().join("private-target");
    for (index, checkout) in [&newer, &older, &newer, &older].into_iter().enumerate() {
        let output = succeeds(
            Command::new(env!("CARGO"))
                .current_dir(checkout)
                .env("CARGO_TARGET_DIR", &target)
                .env("CARGO_INCREMENTAL", "0")
                .args([
                    "check",
                    "--offline",
                    "-p",
                    "seedbox-api",
                    "--message-format=json",
                ]),
        );
        let spec = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|message| {
                message["reason"] == "compiler-artifact"
                    && message["target"]["name"] == "seedbox_spec"
            })
            .expect("spec compile artifact");
        assert_eq!(
            spec["fresh"], false,
            "spec metadata must rebuild at switch {index}"
        );
        println!("compile-only worktree switch {index}: {}: current spec/API layout accepted; spec rebuilt", checkout.display());
    }
}
