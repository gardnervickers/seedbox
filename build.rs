use sha2::{Digest, Sha256};
use std::{fs, path::Path};

fn main() {
    let mut files = vec![
        "Cargo.toml".to_owned(),
        "Cargo.lock".to_owned(),
        "flake.lock".to_owned(),
        "build.rs".to_owned(),
    ];
    fn gather(dir: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("read Rust source directory") {
            let path = entry.expect("read source entry").path();
            if path.is_dir() {
                gather(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path.to_string_lossy().into_owned());
            }
        }
    }
    gather(Path::new("src"), &mut files);
    files.sort();
    let mut digest = Sha256::new();
    for file in files {
        println!("cargo:rerun-if-changed={file}");
        digest.update(file.as_bytes());
        digest.update([0]);
        digest.update(fs::read(file).expect("read runner source"));
        digest.update([0]);
    }
    println!(
        "cargo:rustc-env=SEEDBOX_SOURCE_SHA256={:x}",
        digest.finalize()
    );
    let version = std::process::Command::new("pkg-config")
        .args(["--modversion", "unicorn"])
        .output()
        .expect("pkg-config Unicorn version");
    assert!(
        version.status.success(),
        "Unicorn must be supplied by the project development shell"
    );
    println!(
        "cargo:rustc-env=SEEDBOX_UNICORN_VERSION={}",
        String::from_utf8(version.stdout)
            .expect("Unicorn version UTF-8")
            .trim()
    );
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compiler = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .expect("Rust compiler version");
    assert!(
        compiler.status.success(),
        "Rust compiler version query failed"
    );
    println!(
        "cargo:rustc-env=SEEDBOX_RUSTC_VERSION={}",
        String::from_utf8(compiler.stdout)
            .expect("Rust compiler version UTF-8")
            .trim()
    );
}
