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
    println!("cargo::rustc-check-cfg=cfg(seedbox_embedded_init)");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest/compose-init.c");
    println!("cargo::rerun-if-changed={}", source.display());
    println!("cargo::rerun-if-env-changed=SEEDBOX_STATIC_CC");
    println!("cargo::rerun-if-env-changed=PATH");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "linux" || target_arch != "x86_64" {
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
