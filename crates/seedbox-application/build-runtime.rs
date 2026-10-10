//! Native CLI build setup. Guest assets are always built explicitly outside Cargo.

use std::{
    env,
    path::{Path, PathBuf},
};

fn main() {
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo package directory"));
    let root = std::fs::canonicalize(manifest.join("../..")).expect("source checkout");
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_ROOT");
    let configured =
        env::var_os("SEEDBOX_BUILD_ROOT").and_then(|path| std::fs::canonicalize(path).ok());
    if configured.as_ref() != Some(&root) {
        // Outside the checkout config, recheck inputs on every invocation. The
        // runtime manifest still selects the source when Cargo reuses this script.
        let output = env::var_os("OUT_DIR").expect("Cargo build output directory");
        println!(
            "cargo::rerun-if-changed={}",
            Path::new(&output)
                .join("seedbox-unconfigured-source-cache")
                .display()
        );
    }
    // Consume only already-built checkout assets. Never invoke a builder here.
    let assets = root.join("build/assets");
    println!("cargo::rerun-if-changed={}", assets.display());
    if assets.exists() {
        let architecture =
            std::fs::read_to_string(assets.join("architecture")).expect("asset architecture");
        assert_eq!(architecture.trim(), env::var("CARGO_CFG_TARGET_ARCH").expect("target architecture"), "guest assets target a different architecture; run cargo xtask build-assets on the target build host");
    }
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_HELPERS");
    let helpers = match env::var_os("SEEDBOX_BUILD_HELPERS").or_else(|| {
        assets
            .exists()
            .then(|| assets.join("runtime.tar.gz").into_os_string())
    }) {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo::rerun-if-changed={}", path.display());
            let path = std::fs::canonicalize(&path).unwrap_or_else(|error| {
                panic!("guest helpers {}: {error}; run `cargo xtask build-assets` or set SEEDBOX_BUILD_HELPERS to an explicit archive", path.display())
            });
            format!("pub static EMBEDDED: Option<&[u8]> = Some(include_bytes!({path:?}));\n")
        }
        None => "pub static EMBEDDED: Option<&[u8]> = None;\n".to_owned(),
    };
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_KERNEL");
    let kernel = match env::var_os("SEEDBOX_BUILD_KERNEL").or_else(|| {
        assets
            .exists()
            .then(|| assets.join("kernel").into_os_string())
    }) {
        Some(path) => {
            let path = std::fs::canonicalize(&path).expect("explicit guest kernel");
            println!("cargo::rerun-if-changed={}", path.display());
            format!("pub static EMBEDDED_KERNEL: Option<&[u8]> = Some(include_bytes!({path:?}));\n")
        }
        None => "pub static EMBEDDED_KERNEL: Option<&[u8]> = None;\n".to_owned(),
    };
    std::fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"))
            .join("guest-helpers.rs"),
        helpers + &kernel,
    )
    .expect("write embedded helper declaration");
}
