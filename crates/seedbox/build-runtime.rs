//! Embed the static guest supervisor (`guest/compose-init.c`) so `seedbox run`
//! works without a separately built `--guest-init`. The supervisor must be
//! linked against a libc whose user/group lookups work without NSS modules,
//! so only a musl toolchain is used: `SEEDBOX_STATIC_CC` (set by the Nix shell)
//! or `musl-gcc`. Without one the CLI still builds and requires `--guest-init`.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
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
    println!("cargo::rerun-if-env-changed=SEEDBOX_MAC_RUNTIME_BUNDLE");
    let bundle = env::var_os("SEEDBOX_MAC_RUNTIME_BUNDLE").map(PathBuf::from);
    let declaration = if let Some(bundle) = bundle {
        let bundle =
            std::fs::canonicalize(bundle).expect("Mac runtime bundle must exist at CLI build time");
        println!("cargo::rerun-if-changed={}", bundle.display());
        format!(
            "pub const MAC_RUNTIME:Option<&[u8]>=Some(include_bytes!({:?}));\n",
            bundle
        )
    } else {
        "pub const MAC_RUNTIME:Option<&[u8]>=None;\n".into()
    };
    std::fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("mac-runtime.rs"),
        declaration,
    )
    .unwrap();
    println!("cargo::rerun-if-env-changed=SEEDBOX_LINUX_RUNTIME_BUNDLE");
    let linux_bundle = env::var_os("SEEDBOX_LINUX_RUNTIME_BUNDLE").map(PathBuf::from);
    let linux_declaration = if let Some(bundle) = linux_bundle {
        let bundle =
            std::fs::canonicalize(bundle).expect("Linux guest bundle must exist at CLI build time");
        println!("cargo::rerun-if-changed={}", bundle.display());
        format!(
            "pub const LINUX_RUNTIME:Option<&[u8]>=Some(include_bytes!({:?}));\n",
            bundle
        )
    } else {
        "pub const LINUX_RUNTIME:Option<&[u8]>=None;\n".into()
    };
    std::fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("linux-runtime.rs"),
        linux_declaration,
    )
    .unwrap();
    println!("cargo::rustc-check-cfg=cfg(seedbox_embedded_init)");
    let source = root.join("guest/compose-init.c");
    println!("cargo::rerun-if-changed={}", source.display());
    println!(
        "cargo::rerun-if-changed={}",
        source.with_file_name("control.h").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        source.with_file_name("barrier.h").display()
    );
    println!("cargo::rerun-if-env-changed=SEEDBOX_STATIC_CC");
    println!(
        "cargo::rerun-if-changed={}",
        source.with_file_name("environment.h").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        source.with_file_name("io.h").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        source
            .with_file_name("../kernel-arm64/supervisor-io.h")
            .display()
    );
    println!("cargo::rerun-if-env-changed=PATH");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "linux" || !matches!(target_arch.as_str(), "x86_64" | "aarch64") {
        return;
    }
    let Some(compiler) = env::var_os("SEEDBOX_STATIC_CC")
        .map(PathBuf::from)
        .or_else(|| on_path("musl-gcc"))
    else {
        println!(
            "cargo::warning=no musl compiler (SEEDBOX_STATIC_CC or musl-gcc); \
             `seedbox run` will require --guest-init"
        );
        return;
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("compose-init");
    let status = Command::new(&compiler)
        .args(["-O2", "-static", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&output)
        .arg(&source)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo::rustc-cfg=seedbox_embedded_init");
        }
        other => println!(
            "cargo::warning=building guest supervisor with {} failed ({other:?}); \
             `seedbox run` will require --guest-init",
            compiler.display()
        ),
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}
