//! Execute the actual Cargo build script in a private mock Cargo environment.
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

#[test]
fn build_script_selects_explicit_or_prepared_inputs_without_running_builders() {
    let directory = tempfile::tempdir().unwrap();
    let compiler = directory.path().join("build-script");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("build-runtime.rs");
    let output = Command::new("rustc")
        .arg(source)
        .arg("-o")
        .arg(&compiler)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for case in [
        "explicit-linux",
        "explicit-mac",
        "prepared",
        "override",
        "wrong-arch",
        "missing",
        "ambient",
    ] {
        let root = directory.path().join(case);
        fs::create_dir(&root).unwrap();
        let checkout = root.join("checkout");
        let manifest = checkout.join("crates/seedbox-application");
        fs::create_dir_all(&manifest).unwrap();
        fs::write(
            manifest.join("Cargo.toml"),
            "[package]\nname = 'seedbox-application'\nversion = '0.0.0'\n",
        )
        .unwrap();
        let out = root.join("output");
        fs::create_dir(&out).unwrap();
        let tools = root.join("forbidden-tools");
        fs::create_dir(&tools).unwrap();
        let marker = root.join("builder-invoked");
        for tool in ["nix", "docker", "podman", "python3", "cc", "git"] {
            let path = tools.join(tool);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\necho {tool} >> '{}'\nexit 97\n",
                    marker.display()
                ),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut environment: BTreeMap<_, _> = std::env::vars()
            .filter(|(k, _)| !k.starts_with("SEEDBOX_"))
            .collect();
        for (key, value) in [
            ("CARGO_MANIFEST_DIR", manifest.clone()),
            ("OUT_DIR", out.clone()),
            ("PATH", tools.clone()),
            ("SEEDBOX_STATIC_CC", tools.join("cc")),
            ("SEEDBOX_BUILDER", tools.join("docker")),
        ] {
            environment.insert(key.into(), value.to_str().unwrap().into());
        }
        environment.insert("PROFILE".into(), "release".into());
        environment.insert(
            "CARGO_CFG_TARGET_OS".into(),
            if case == "explicit-linux" || case == "wrong-arch" {
                "linux"
            } else {
                "macos"
            }
            .into(),
        );
        environment.insert(
            "CARGO_CFG_TARGET_ARCH".into(),
            if case == "explicit-linux" || case == "wrong-arch" {
                "x86_64"
            } else {
                "aarch64"
            }
            .into(),
        );
        let prepared = checkout.join("build/assets");
        if ["prepared", "override", "wrong-arch"].contains(&case) {
            fs::create_dir_all(&prepared).unwrap();
            fs::write(prepared.join("architecture"), "aarch64\n").unwrap();
            fs::write(prepared.join("runtime.tar.gz"), "prepared helpers").unwrap();
            fs::write(prepared.join("kernel"), "prepared kernel").unwrap();
        }
        let helpers = root.join("helpers.tar.gz");
        let kernel = root.join("kernel");
        if ["explicit-linux", "explicit-mac", "override"].contains(&case) {
            fs::write(&helpers, "explicit helpers").unwrap();
            fs::write(&kernel, "explicit kernel").unwrap();
            environment.insert(
                "SEEDBOX_BUILD_HELPERS".into(),
                helpers.to_str().unwrap().into(),
            );
            environment.insert(
                "SEEDBOX_BUILD_KERNEL".into(),
                kernel.to_str().unwrap().into(),
            );
        } else if case == "missing" {
            environment.insert(
                "SEEDBOX_BUILD_HELPERS".into(),
                helpers.to_str().unwrap().into(),
            );
        } else if case == "ambient" {
            let cache = root.join("cache/aarch64");
            fs::create_dir_all(&cache).unwrap();
            fs::write(cache.join("runtime.tar.gz"), "stale archive").unwrap();
            environment.insert(
                "SEEDBOX_ASSET_DIR".into(),
                root.join("cache").to_str().unwrap().into(),
            );
        }
        let output = Command::new(&compiler)
            .env_clear()
            .envs(environment)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!marker.exists(), "{case}: a builder was invoked");
        assert!(!out.join("compose-init").exists());
        if ["wrong-arch", "missing"].contains(&case) {
            assert!(!output.status.success(), "{case}");
            assert!(
                stderr.contains("cargo xtask build-assets"),
                "{case}: {stderr}"
            );
            assert!(
                stderr.contains(if case == "wrong-arch" {
                    "guest assets target a different architecture"
                } else {
                    "helpers.tar.gz"
                }),
                "{stderr}"
            );
            assert!(!out.join("guest-helpers.rs").exists());
        } else {
            assert!(output.status.success(), "{case}: {stderr}");
            let generated = fs::read_to_string(out.join("guest-helpers.rs")).unwrap();
            if case == "ambient" {
                assert_eq!(generated, "pub static EMBEDDED: Option<&[u8]> = None;\npub static EMBEDDED_KERNEL: Option<&[u8]> = None;\n");
            } else {
                let (helpers, kernel) = if case == "prepared" {
                    (prepared.join("runtime.tar.gz"), prepared.join("kernel"))
                } else {
                    (helpers, kernel)
                };
                for path in [helpers, kernel] {
                    assert!(
                        generated.contains(path.canonicalize().unwrap().to_str().unwrap()),
                        "{case}: {generated}"
                    );
                }
                if case == "override" {
                    assert!(!generated.contains(prepared.to_str().unwrap()));
                }
            }
            assert!(!String::from_utf8_lossy(&output.stdout).contains("SEEDBOX_RELEASE_ASSETS"));
        }
    }
}
