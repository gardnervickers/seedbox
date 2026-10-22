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
    // Only complete executable builds request embedding. Portable Cargo builds
    // must not depend on an existing (or subsequently published) asset bundle.
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"));
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_DRIVER_RUNTIME");
    let driver = env::var_os("SEEDBOX_BUILD_DRIVER_RUNTIME").map(PathBuf::from);
    let declaration = match driver {
        Some(path) => {
            println!("cargo::rerun-if-changed={}", path.display());
            let path = std::fs::canonicalize(&path).unwrap_or_else(|error| {
                panic!("driver runtime {}: {error}; run cargo xtask build-assets to prepare complete runtime inputs", path.display())
            });
            let architecture = path.parent().expect("runtime parent").join("architecture");
            if architecture.is_file() {
                println!("cargo::rerun-if-changed={}", architecture.display());
                let architecture =
                    std::fs::read_to_string(architecture).expect("asset architecture");
                assert_eq!(architecture.trim(), env::var("CARGO_CFG_TARGET_ARCH").expect("target architecture"), "guest assets target a different architecture; run cargo xtask build-assets on the target build host");
            }
            format!("pub static EMBEDDED: Option<&[u8]> = Some(include_bytes!({path:?}));\n")
        }
        None => "pub static EMBEDDED: Option<&[u8]> = None;\n".to_owned(),
    };
    std::fs::write(output.join("driver-runtime.rs"), declaration)
        .expect("write embedded Driver runtime declaration");
}
