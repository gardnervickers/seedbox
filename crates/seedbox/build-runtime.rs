//! Signing belongs to the executable that hosts the native worker. The build
//! also records the source revision runs report as engine provenance.
use std::{path::Path, process::Command};

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        println!("cargo::rustc-link-arg-bin=seedbox=--seedbox-sign-hvf");
    }
    println!(
        "cargo::rustc-env=SEEDBOX_BUILD_REVISION={}",
        revision().unwrap_or_default()
    );
}

/// `HEAD`, suffixed `-dirty` when tracked files differ from it. Builds without
/// Git metadata, such as a source archive, record no revision. Cargo reruns
/// this when `HEAD`, its branch or the index changes; edits that have not
/// touched the index since the last build keep the earlier dirty state.
fn revision() -> Option<String> {
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
    };
    let head = git(&["rev-parse", "HEAD"])?;
    let mut watched = vec![
        "HEAD".to_owned(),
        "index".to_owned(),
        "packed-refs".to_owned(),
    ];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(branch);
    }
    for name in watched {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", &name]) {
            if Path::new(&path).exists() {
                println!("cargo::rerun-if-changed={path}");
            }
        }
    }
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])?;
    Some(if dirty.is_empty() {
        head
    } else {
        format!("{head}-dirty")
    })
}
