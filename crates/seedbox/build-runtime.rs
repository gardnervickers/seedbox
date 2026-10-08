//! Embed packaged release guest assets and the native development supervisor.
//! Cargo delegates guest assets to the cached Linux build pipeline. Unbundled
//! Linux development builds retain the musl supervisor fallback, whose user/group
//! lookups must work without NSS modules.

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
    kernel_release(&root);
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        // Only the public CLI (which also hosts retained workers) needs HVF.
        println!("cargo::rustc-link-arg-bin=seedbox=--seedbox-sign-hvf");
    }
    // Release builds own guest assets. Debug builds remain portable for readers
    // and unit tests and do not require a container engine.
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let bundle = output.join("runtime.tar.gz");
    let release = env::var("PROFILE").as_deref() == Ok("release");
    let supported = matches!(
        (
            env::var("CARGO_CFG_TARGET_OS").as_deref(),
            env::var("CARGO_CFG_TARGET_ARCH").as_deref()
        ),
        (Ok("linux"), Ok("x86_64")) | (Ok("macos"), Ok("aarch64"))
    );
    let declaration = if release && supported {
        for input in [
            "flake.nix",
            "flake.lock",
            "nix",
            "guest",
            "kernel",
            "kernel-arm64",
            "tools/runtime",
            "tools/linux-runtime/package.py",
        ] {
            println!("cargo::rerun-if-changed={}", root.join(input).display());
        }
        let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("aarch64") => "arm64",
            Ok("x86_64") => "amd64",
            _ => panic!("unsupported guest build architecture"),
        };
        let status = Command::new("python3")
            .arg(root.join("tools/runtime/build.py"))
            .arg(&bundle)
            .arg(arch)
            .status()
            .unwrap_or_else(|error| {
                eprintln!("guest assets: cannot run python3: {error}; enter nix develop");
                std::process::exit(1);
            });
        if !status.success() {
            eprintln!("guest asset preparation failed; see build output above");
            std::process::exit(1);
        }
        // Keep the large archive in static storage; a const expands at use sites
        // and can make compiler constant evaluation consume excessive memory.
        format!(
            "pub static RUNTIME: Option<&[u8]> = Some(include_bytes!({:?}));\n",
            bundle
        )
    } else {
        "pub static RUNTIME: Option<&[u8]> = None;\n".into()
    };
    std::fs::write(output.join("container-runtime.rs"), declaration)
        .expect("write runtime declaration");
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
        // The package already contains the Linux-built supervisor. Native
        // development builds keep the legacy embedded-supervisor fallback.
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

fn git(root: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn simulator_version(root: &Path) {
    println!("cargo::rerun-if-changed=build_version.rs");
    println!("cargo::rerun-if-env-changed=SEEDBOX_SOURCE_REVISION");
    println!("cargo::rerun-if-env-changed=SEEDBOX_SOURCE_DIRTY");
    let explicit = env::var("SEEDBOX_SOURCE_REVISION").ok();
    let (revision, dirty, tag) = if let Some(revision) = explicit {
        let dirty = env::var("SEEDBOX_SOURCE_DIRTY").as_deref() != Ok("0");
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
    );
    println!("cargo::rustc-env=SEEDBOX_SIMULATOR_VERSION={version}");
}

fn on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

/// The distribution address follows source inputs, never the host or Git HEAD.
/// Keep this framing identical to kernel/release.py (checked by portable tests).
fn kernel_release(root: &Path) {
    use sha2::{Digest, Sha256};
    let groups: std::collections::BTreeMap<String, Vec<String>> = serde_json::from_slice(
        &std::fs::read(root.join("kernel/inputs.json")).expect("kernel inputs"),
    )
    .expect("kernel input groups");
    let names: std::collections::BTreeSet<_> = groups.into_values().flatten().collect();
    let mut hash = Sha256::new();
    hash.update(b"seedbox-kernel-v1\0");
    for name in names {
        println!("cargo::rerun-if-changed={}", root.join(&name).display());
        let content = std::fs::read(root.join(&name)).expect("kernel recipe input");
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update((content.len() as u64).to_be_bytes());
        hash.update(content);
    }
    println!(
        "cargo::rustc-env=SEEDBOX_KERNEL_RELEASE={:x}",
        hash.finalize()
    );
}
