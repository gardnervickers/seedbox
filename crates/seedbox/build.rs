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
mod build_version;

fn main() {
    simulator_version();
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
    println!("cargo::rustc-check-cfg=cfg(seedbox_embedded_init)");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest/compose-init.c");
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

fn git(root: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8(output.stdout)
            .expect("Git build metadata must be UTF-8")
            .trim()
            .to_owned()
    })
}

fn simulator_version() {
    println!("cargo::rerun-if-changed=build_version.rs");
    println!("cargo::rerun-if-env-changed=SEEDBOX_SOURCE_REVISION");
    println!("cargo::rerun-if-env-changed=SEEDBOX_SOURCE_DIRTY");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let explicit = env::var("SEEDBOX_SOURCE_REVISION").ok();
    let (revision, dirty, tag) = if let Some(revision) = explicit {
        let dirty = match env::var("SEEDBOX_SOURCE_DIRTY").as_deref() {
            Ok("0") => false,
            Ok("1") => true,
            _ => panic!("SEEDBOX_SOURCE_REVISION requires SEEDBOX_SOURCE_DIRTY=0 or 1"),
        };
        // Exported build sources have no release-tag proof.
        (Some(revision), dirty, None)
    } else {
        for name in ["HEAD", "index", "packed-refs"] {
            if let Some(path) = git(&root, &["rev-parse", "--git-path", name]) {
                println!("cargo::rerun-if-changed={path}");
            }
        }
        if let Some(branch) = git(&root, &["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(&root, &["rev-parse", "--git-path", &branch]) {
                println!("cargo::rerun-if-changed={path}");
            }
        }
        if let Some(files) = git(&root, &["ls-files", "-z"]) {
            for file in files.split('\0').filter(|file| !file.is_empty()) {
                println!("cargo::rerun-if-changed={}", root.join(file).display());
            }
        }
        (
            git(&root, &["rev-parse", "--verify", "HEAD"]),
            git(&root, &["status", "--porcelain"]).is_none_or(|status| !status.is_empty()),
            git(&root, &["describe", "--exact-match", "--tags", "HEAD"]),
        )
    };
    let version = build_version::label(
        env!("CARGO_PKG_VERSION"),
        revision.as_deref(),
        dirty,
        tag.as_deref(),
    )
    .expect("invalid simulator build provenance");
    println!("cargo::rustc-env=SEEDBOX_SIMULATOR_VERSION={version}");
}

fn on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}
