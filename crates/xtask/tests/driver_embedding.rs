//! Host-only build-script contract tests. These compile/execute ordinary Rust
//! fixtures; they do not build or run Linux guests, a hypervisor, or QEMU.
mod support;
use std::{fs, path::PathBuf, process::Command};
use support::{root, run, success};

struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    script: PathBuf,
    output: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("source");
        fs::create_dir_all(root.join("crates/seedbox-driver-execution")).unwrap();
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        let script = directory.path().join("build-script");
        success(
            Command::new("rustc")
                .args(["--edition=2021"])
                .arg(root_source())
                .arg("-o")
                .arg(&script),
            30,
        );
        Self {
            directory,
            root,
            script,
            output,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.script);
        // An empty tool path proves this Driver execution build script cannot need
        // Cargo, Nix, Docker, a kernel builder, formatter or QEMU at runtime.
        command
            .env_clear()
            .env("PATH", "")
            .env(
                "CARGO_MANIFEST_DIR",
                self.root.join("crates/seedbox-driver-execution"),
            )
            .env("OUT_DIR", &self.output)
            .env("SEEDBOX_BUILD_ROOT", &self.root)
            .env("CARGO_CFG_TARGET_ARCH", std::env::consts::ARCH);
        command
    }
    fn assets(&self) -> PathBuf {
        let assets = self.root.join("build/assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("architecture"), std::env::consts::ARCH).unwrap();
        assets
    }
}
fn root_source() -> PathBuf {
    root().join("crates/seedbox-driver-execution/build-runtime.rs")
}

#[test]
fn source_only_checkouts_allow_portable_builds_without_guest_inputs_or_builders() {
    let fixture = Fixture::new();
    success(&mut fixture.command(), 10);
    assert_eq!(
        fs::read_to_string(fixture.output.join("driver-runtime.rs")).unwrap(),
        "pub static EMBEDDED: Option<&[u8]> = None;\n"
    );
    assert!(!fixture.root.join("build").exists());
}
#[test]
fn complete_build_embeds_the_full_package_and_no_longer_needs_source_asset_paths() {
    let fixture = Fixture::new();
    let assets = fixture.assets();
    fs::write(
        assets.join("driver-runtime.tar.gz"),
        b"complete offline runtime fixture",
    )
    .unwrap();
    success(
        fixture.command().env(
            "SEEDBOX_BUILD_DRIVER_RUNTIME",
            assets.join("driver-runtime.tar.gz"),
        ),
        10,
    );
    let generated = fixture.output.join("driver-runtime.rs");
    let declaration = fs::read_to_string(&generated).unwrap();
    assert!(declaration.contains(&assets.join("driver-runtime.tar.gz").display().to_string()));
    let source = fixture.directory.path().join("embedded.rs");
    fs::write(&source, format!("include!({generated:?}); fn main() {{ assert_eq!(EMBEDDED, Some(b\"complete offline runtime fixture\".as_slice())); }}")).unwrap();
    let executable = fixture.directory.path().join("embedded-fixture");
    success(
        Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&executable),
        30,
    );
    fs::remove_dir_all(assets).unwrap();
    success(&mut Command::new(&executable), 10);
}
#[test]
fn unrequested_assets_neither_embed_nor_invalidate_portable_builds() {
    let fixture = Fixture::new();
    let assets = fixture.assets();
    // Even an incomplete or foreign-architecture publication is irrelevant.
    fs::write(assets.join("architecture"), "wrong-architecture").unwrap();
    for complete in [false, true] {
        if complete {
            fs::write(assets.join("driver-runtime.tar.gz"), b"runtime").unwrap();
        }
        let output = success(&mut fixture.command(), 10);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("build/assets"));
        assert_eq!(
            fs::read_to_string(fixture.output.join("driver-runtime.rs")).unwrap(),
            "pub static EMBEDDED: Option<&[u8]> = None;\n"
        );
    }
}
#[test]
fn explicit_package_override_wins_and_missing_override_is_actionable() {
    let fixture = Fixture::new();
    let assets = fixture.assets();
    fs::write(assets.join("driver-runtime.tar.gz"), b"default").unwrap();
    let explicit = fixture.directory.path().join("override.tar.gz");
    fs::write(&explicit, b"explicit").unwrap();
    success(
        fixture
            .command()
            .env("SEEDBOX_BUILD_DRIVER_RUNTIME", &explicit),
        10,
    );
    let generated = fs::read_to_string(fixture.output.join("driver-runtime.rs")).unwrap();
    assert!(generated.contains(&explicit.display().to_string()));
    assert!(!generated.contains(&assets.display().to_string()));
    fs::remove_file(&explicit).unwrap();
    let output = run(
        fixture
            .command()
            .env("SEEDBOX_BUILD_DRIVER_RUNTIME", &explicit),
        10,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(&explicit.display().to_string()));
}
#[test]
fn target_architecture_mismatch_fails_without_invoking_a_builder() {
    let fixture = Fixture::new();
    let assets = fixture.assets();
    fs::write(assets.join("driver-runtime.tar.gz"), b"package").unwrap();
    fs::write(assets.join("architecture"), "wrong-architecture").unwrap();
    let output = run(
        fixture.command().env(
            "SEEDBOX_BUILD_DRIVER_RUNTIME",
            assets.join("driver-runtime.tar.gz"),
        ),
        10,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("guest assets target a different architecture"));
    assert!(!fixture.output.join("driver-runtime.rs").exists());
}
