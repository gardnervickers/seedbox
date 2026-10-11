fn main() {
    // A reused target directory must not keep a C object from another checkout
    // whose source timestamps happen to be older than the cached build.
    println!("cargo:rerun-if-env-changed=SEEDBOX_BUILD_ROOT");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo package directory");
    let manifest = std::fs::canonicalize(manifest).expect("runtime package directory");
    let expected = manifest.ancestors().nth(2).expect("runtime checkout");
    let configured =
        std::env::var_os("SEEDBOX_BUILD_ROOT").and_then(|path| std::fs::canonicalize(path).ok());
    if configured.as_deref() != Some(expected) {
        let output = std::env::var_os("OUT_DIR").expect("Cargo build output directory");
        println!(
            "cargo:rerun-if-changed={}",
            std::path::Path::new(&output)
                .join("seedbox-unconfigured-source-cache")
                .display()
        );
    }
    println!("cargo:rerun-if-changed=src/backend/hvf_simd.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        let mut build = cc::Build::new();
        if cfg!(target_os = "macos") {
            // This shim targets Apple's Hypervisor SDK. Use the selected Apple
            // toolchain directly: cc's macOS target spelling otherwise triggers
            // Nix cc-wrapper's multi-target warning on native ARM builds.
            println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
            let compiler = std::process::Command::new("/usr/bin/xcrun")
                .args(["--sdk", "macosx", "--find", "clang"])
                .output()
                .expect("locate Apple clang; install Xcode Command Line Tools");
            assert!(
                compiler.status.success(),
                "locate Apple clang: {}",
                String::from_utf8_lossy(&compiler.stderr)
            );
            let compiler = String::from_utf8(compiler.stdout).expect("Apple clang path");
            build.compiler(compiler.trim());
        }
        build
            .file("src/backend/hvf_simd.c")
            .compile("seedbox_hvf_simd");
    }
}
