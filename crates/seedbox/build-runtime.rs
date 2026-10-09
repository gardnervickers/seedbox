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
    // Only release applications embed helpers. Portable debug checks need no
    // guest build, and Cargo never invokes Nix or a container engine.
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_HELPERS");
    println!("cargo::rerun-if-env-changed=SEEDBOX_ASSET_DIR");
    let explicit_helpers = env::var_os("SEEDBOX_BUILD_HELPERS").map(PathBuf::from);
    let explicitly_selected = explicit_helpers.is_some();
    let helpers = if env::var("PROFILE").as_deref() == Ok("release") || explicit_helpers.is_some() {
        let path = explicit_helpers.unwrap_or_else(|| {
            unsafe extern "C" {
                fn geteuid() -> u32;
            }
            let directory = env::var_os("SEEDBOX_ASSET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("/tmp/seedbox-{}", unsafe { geteuid() })));
            directory
                .join(env::var("CARGO_CFG_TARGET_ARCH").expect("target architecture"))
                .join("runtime.tar.gz")
        });
        println!("cargo::rerun-if-changed={}", path.display());
        match std::fs::canonicalize(&path) {
            Ok(path) => format!("pub static EMBEDDED: Option<&[u8]> = Some(include_bytes!({path:?}));\n"),
            Err(error) if !explicitly_selected && error.kind() == std::io::ErrorKind::NotFound => {
                println!("cargo::warning=guest helpers not embedded; run `bash tools/build-assets.sh` and rebuild to make a self-contained CLI");
                "pub static EMBEDDED: Option<&[u8]> = None;\n".into()
            }
            Err(error) => panic!("guest helpers {}: {error}; run `bash tools/build-assets.sh` or set SEEDBOX_BUILD_HELPERS to a helper archive", path.display()),
        }
    } else {
        "pub static EMBEDDED: Option<&[u8]> = None;\n".to_owned()
    };
    std::fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"))
            .join("guest-helpers.rs"),
        helpers,
    )
    .expect("write embedded helper declaration");
}
