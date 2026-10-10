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
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        // Only the public CLI (which also hosts retained workers) needs HVF.
        println!("cargo::rustc-link-arg-bin=seedbox=--seedbox-sign-hvf");
    }
    // Embedding is explicit. Plain Cargo builds never consult ambient caches
    // or invoke Nix or a container engine.
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_HELPERS");
    let helpers = match env::var_os("SEEDBOX_BUILD_HELPERS") {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo::rerun-if-changed={}", path.display());
            let path = std::fs::canonicalize(&path).unwrap_or_else(|error| {
                panic!("guest helpers {}: {error}; run `python3 tools/build.py` or set SEEDBOX_BUILD_HELPERS to an explicit archive", path.display())
            });
            format!("pub static EMBEDDED: Option<&[u8]> = Some(include_bytes!({path:?}));\n")
        }
        None => "pub static EMBEDDED: Option<&[u8]> = None;\n".to_owned(),
    };
    std::fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"))
            .join("guest-helpers.rs"),
        helpers,
    )
    .expect("write embedded helper declaration");
}
