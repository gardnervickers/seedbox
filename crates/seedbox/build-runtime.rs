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
    simulator_version(&root);
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
        .arg("--no-optional-locks")
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

fn simulator_version(root: &Path) {
    println!("cargo::rerun-if-changed=build_version.rs");
    println!("cargo::rerun-if-env-changed=SEEDBOX_SOURCE_REVISION");
    println!("cargo::rerun-if-env-changed=SEEDBOX_SOURCE_DIRTY");
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
        // Git may return repository-relative paths in ordinary clones. Cargo
        // interprets watchers relative to this crate, so request absolute paths.
        // Missing files force Cargo to rebuild forever; refs directories cover
        // creation of loose refs after packing and release-tag-only changes.
        for name in ["HEAD", "index", "packed-refs", "refs/heads", "refs/tags"] {
            if let Some(path) = git(
                root,
                &["rev-parse", "--path-format=absolute", "--git-path", name],
            ) {
                if Path::new(&path).exists() {
                    println!("cargo::rerun-if-changed={path}");
                }
            }
        }
        if let Some(branch) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(
                root,
                &["rev-parse", "--path-format=absolute", "--git-path", &branch],
            ) {
                if Path::new(&path).exists() {
                    println!("cargo::rerun-if-changed={path}");
                }
            }
        }
        if let Some(files) = git(root, &["ls-files", "-z"]) {
            for file in files.split('\0').filter(|file| !file.is_empty()) {
                println!("cargo::rerun-if-changed={}", root.join(file).display());
            }
        }
        (
            git(root, &["rev-parse", "--verify", "HEAD"]),
            git(root, &["status", "--porcelain", "--untracked-files=no"])
                .is_none_or(|status| !status.is_empty()),
            git(root, &["describe", "--exact-match", "--tags", "HEAD"]),
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
