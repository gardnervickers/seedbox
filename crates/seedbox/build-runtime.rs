//! Select released guest assets and embed the native development supervisor.
//! Release builds never download or compile guest assets. Unbundled
//! Linux development builds retain the musl supervisor fallback, whose user/group
//! lookups must work without NSS modules.

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
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        // Only the public CLI (which also hosts retained workers) needs HVF.
        println!("cargo::rustc-link-arg-bin=seedbox=--seedbox-sign-hvf");
    }
    // Release builds use independent guest releases. Debug builds retain native
    // development fallbacks for portable readers and tests.
    let release = env::var("PROFILE").as_deref() == Ok("release");
    let supported = matches!(
        (
            env::var("CARGO_CFG_TARGET_OS").as_deref(),
            env::var("CARGO_CFG_TARGET_ARCH").as_deref()
        ),
        (Ok("linux"), Ok("x86_64")) | (Ok("macos"), Ok("aarch64"))
    );
    println!(
        "cargo::rustc-env=SEEDBOX_RELEASE_ASSETS={}",
        u8::from(release && supported)
    );
    println!("cargo::rerun-if-env-changed=SEEDBOX_LINUX_RUNTIME_BUNDLE");
    let linux_bundle = env::var_os("SEEDBOX_LINUX_RUNTIME_BUNDLE").map(PathBuf::from);
    let linux_declaration = if let Some(bundle) = linux_bundle {
        let bundle =
            std::fs::canonicalize(bundle).expect("Linux guest bundle must exist at CLI build time");
        println!("cargo::rerun-if-changed={}", bundle.display());
        format!(
            "pub static LINUX_RUNTIME:Option<&[u8]>=Some(include_bytes!({:?}));\n",
            bundle
        )
    } else {
        "pub static LINUX_RUNTIME:Option<&[u8]>=None;\n".into()
    };
    std::fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("linux-runtime.rs"),
        linux_declaration,
    )
    .unwrap();
    println!("cargo::rustc-check-cfg=cfg(seedbox_embedded_init)");
    if release && supported {
        // Release runs download Linux-built helpers. Native development builds
        // keep the existing embedded-supervisor fallback.
        return;
    }
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
