//! Cargo consumes prepared guest assets without ever running guest builders.
#![cfg(unix)]
mod support;

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use support::{diagnostic, root, run, success};

struct Fixture {
    directory: tempfile::TempDir,
    checkout: PathBuf,
    output: PathBuf,
    marker: PathBuf,
    environment: BTreeMap<OsString, OsString>,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        // The fixture owns the compiler output too: process-static TempDirs
        // are never dropped, so even a shared successful build would leak.
        success(
            Command::new("rustc")
                .arg(root().join("crates/seedbox-application/build-runtime.rs"))
                .arg("-o")
                .arg(directory.path().join("build-script")),
            120,
        );
        let checkout = directory.path().join("checkout");
        let manifest = checkout.join("crates/seedbox-application");
        fs::create_dir_all(&manifest).unwrap();
        fs::write(
            manifest.join("Cargo.toml"),
            "[package]\nname = \"seedbox-application\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        let marker = directory.path().join("guest-build-invoked");
        let tools = directory.path().join("forbidden-tools");
        fs::create_dir(&tools).unwrap();
        for name in ["nix", "docker", "podman", "python3", "cc", "git"] {
            let executable = tools.join(name);
            fs::write(
                &executable,
                format!(
                    "#!/bin/sh\necho {name} >> '{}'\nexit 97\n",
                    marker.display()
                ),
            )
            .unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let environment = std::env::vars_os()
            .filter(|(key, _)| !key.to_string_lossy().starts_with("SEEDBOX_"))
            .collect();
        let mut fixture = Self {
            directory,
            checkout,
            output,
            marker,
            environment,
        };
        for (key, value) in [
            ("CARGO_MANIFEST_DIR", manifest.into_os_string()),
            ("OUT_DIR", fixture.output.clone().into_os_string()),
            ("PROFILE", "release".into()),
            ("CARGO_CFG_TARGET_OS", "macos".into()),
            ("CARGO_CFG_TARGET_ARCH", "aarch64".into()),
            ("PATH", tools.clone().into_os_string()),
            ("SEEDBOX_STATIC_CC", tools.join("cc").into_os_string()),
            ("SEEDBOX_BUILDER", tools.join("docker").into_os_string()),
        ] {
            fixture.environment.insert(key.into(), value);
        }
        fixture
    }

    fn prepared(&self, architecture: &str) -> PathBuf {
        let assets = self.checkout.join("build/assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("architecture"), format!("{architecture}\n")).unwrap();
        fs::write(assets.join("runtime.tar.gz"), b"prepared helpers").unwrap();
        fs::write(assets.join("kernel"), b"prepared kernel").unwrap();
        fs::write(
            assets.join("driver-runtime.tar.gz"),
            b"prepared driver runtime",
        )
        .unwrap();
        assets
    }

    fn explicit(&mut self, prefix: &str) -> (PathBuf, PathBuf) {
        let helpers = self
            .directory
            .path()
            .join(format!("{prefix}-helpers.tar.gz"));
        let kernel = self.directory.path().join(format!("{prefix}-kernel"));
        fs::write(&helpers, format!("{prefix} helpers")).unwrap();
        fs::write(&kernel, format!("{prefix} kernel")).unwrap();
        self.environment
            .insert("SEEDBOX_BUILD_HELPERS".into(), helpers.clone().into());
        self.environment
            .insert("SEEDBOX_BUILD_KERNEL".into(), kernel.clone().into());
        (helpers, kernel)
    }

    fn run(&self) -> Output {
        let result = run(
            Command::new(self.directory.path().join("build-script"))
                .env_clear()
                .envs(&self.environment),
            30,
        );
        assert!(!self.marker.exists(), "{}", diagnostic(&result));
        assert!(!self.output.join("compose-init").exists());
        result
    }

    fn embedded(&self) -> String {
        fs::read_to_string(self.output.join("guest-helpers.rs")).unwrap()
    }

    fn embeds(&self, path: &Path) {
        assert!(self
            .embedded()
            .contains(path.canonicalize().unwrap().to_str().unwrap()));
    }
}

#[test]
fn release_build_script_never_invokes_guest_builders() {
    let mut fixture = Fixture::new();
    let (helpers, kernel) = fixture.explicit("explicit");
    for (os, arch) in [("linux", "x86_64"), ("macos", "aarch64")] {
        fixture
            .environment
            .insert("CARGO_CFG_TARGET_OS".into(), os.into());
        fixture
            .environment
            .insert("CARGO_CFG_TARGET_ARCH".into(), arch.into());
        let output = fixture.run();
        assert!(
            output.status.success(),
            "{os}/{arch}: {}",
            diagnostic(&output)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SEEDBOX_RELEASE_ASSETS"));
        fixture.embeds(&helpers);
        fixture.embeds(&kernel);
    }
    let directory = fixture.directory.path().to_owned();
    let compiler_artifact = directory.join("build-script");
    assert!(compiler_artifact.is_file());
    drop(fixture);
    assert!(
        !compiler_artifact.exists(),
        "compiler artifact must be cleaned up"
    );
    assert!(!directory.exists(), "fixture directory must be cleaned up");
}

#[test]
fn prepared_checkout_assets_are_embedded() {
    let fixture = Fixture::new();
    let assets = fixture.prepared("aarch64");
    let output = fixture.run();
    assert!(output.status.success(), "{}", diagnostic(&output));
    fixture.embeds(&assets.join("runtime.tar.gz"));
    fixture.embeds(&assets.join("kernel"));
}

#[test]
fn explicit_assets_override_prepared_checkout_assets() {
    let mut fixture = Fixture::new();
    let assets = fixture.prepared("aarch64");
    let (helpers, kernel) = fixture.explicit("override");
    let output = fixture.run();
    assert!(output.status.success(), "{}", diagnostic(&output));
    fixture.embeds(&helpers);
    fixture.embeds(&kernel);
    assert!(!fixture
        .embedded()
        .contains(assets.canonicalize().unwrap().to_str().unwrap()));
}

#[test]
fn prepared_assets_for_another_architecture_are_rejected() {
    let mut fixture = Fixture::new();
    fixture.prepared("aarch64");
    fixture
        .environment
        .insert("CARGO_CFG_TARGET_OS".into(), "linux".into());
    fixture
        .environment
        .insert("CARGO_CFG_TARGET_ARCH".into(), "x86_64".into());
    let output = fixture.run();
    assert!(!output.status.success(), "{}", diagnostic(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("guest assets target a different architecture"),
        "{stderr}"
    );
    assert!(stderr.contains("cargo xtask build-assets"), "{stderr}");
    assert!(!fixture.output.join("guest-helpers.rs").exists());
}

#[test]
fn missing_release_helpers_explain_the_explicit_build() {
    let mut fixture = Fixture::new();
    fixture.environment.insert(
        "SEEDBOX_BUILD_HELPERS".into(),
        fixture.directory.path().join("missing.tar.gz").into(),
    );
    let output = fixture.run();
    assert!(!output.status.success(), "{}", diagnostic(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cargo xtask build-assets"), "{stderr}");
    assert!(stderr.contains("missing.tar.gz"), "{stderr}");
}

#[test]
fn ambient_cached_helpers_are_never_embedded() {
    let mut fixture = Fixture::new();
    let cache = fixture.directory.path().join("ambient-cache");
    let stale = cache.join("aarch64/runtime.tar.gz");
    fs::create_dir_all(stale.parent().unwrap()).unwrap();
    fs::write(stale, b"stale archive").unwrap();
    fixture
        .environment
        .insert("SEEDBOX_ASSET_DIR".into(), cache.into());
    let output = fixture.run();
    assert!(output.status.success(), "{}", diagnostic(&output));
    assert_eq!(
        fixture.embedded(),
        "pub static EMBEDDED: Option<&[u8]> = None;\npub static EMBEDDED_KERNEL: Option<&[u8]> = None;\n"
    );
}
