//! Protocol data crates must not acquire execution ownership through any
//! normal/dev/build/optional/target-specific machine or Driver dependency.
use std::{path::PathBuf, process::Command};
#[test]
fn canonical_and_standalone_protocols_have_no_execution_dependency_edges() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (manifest, protocol) in [
        ("Cargo.toml", "seedbox-engine-protocol"),
        ("crates/seedbox-vmm/Cargo.toml", "seedbox-vmm-protocol"),
        ("crates/seedbox-arm64/Cargo.toml", "seedbox-arm64-protocol"),
    ] {
        let output = Command::new(env!("CARGO"))
            .current_dir(&root)
            .args([
                "metadata",
                "--no-deps",
                "--locked",
                "--offline",
                "--format-version",
                "1",
                "--manifest-path",
            ])
            .arg(root.join(manifest))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{manifest}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let package = metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["name"] == protocol)
            .expect("protocol must be a member of its owning workspace");
        for dependency in package["dependencies"].as_array().unwrap() {
            let name = dependency["name"].as_str().unwrap();
            assert!(
                ![
                    "seedbox-machine",
                    "seedbox-machine-pv",
                    "seedbox-machine-tcg",
                    "seedbox-driver",
                    "seedbox-engine-core",
                    "seedbox-vmm-engine-core",
                    "seedbox-arm64-engine-core"
                ]
                .contains(&name),
                "{protocol} has forbidden execution dependency {dependency}"
            );
        }
    }
}
