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
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"));
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_DRIVER_RUNTIME");
    let driver = env::var_os("SEEDBOX_BUILD_DRIVER_RUNTIME")
        .map(PathBuf::from)
        .or_else(|| {
            assets
                .exists()
                .then(|| assets.join("driver-runtime.tar.gz"))
        });
    let declaration = match driver {
        Some(path) => {
            println!("cargo::rerun-if-changed={}", path.display());
            let path = std::fs::canonicalize(&path).unwrap_or_else(|error| {
                panic!("driver runtime {}: {error}; run cargo xtask build-assets to prepare complete runtime inputs", path.display())
            });
            format!("pub static EMBEDDED: Option<&[u8]> = Some(include_bytes!({path:?}));\n")
        }
        None => "pub static EMBEDDED: Option<&[u8]> = None;\n".to_owned(),
    };
    std::fs::write(output.join("driver-runtime.rs"), declaration)
        .expect("write embedded Driver runtime declaration");
}
