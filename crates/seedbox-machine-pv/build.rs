#![allow(
    clippy::disallowed_methods,
    reason = "build script runs under cargo, not in a Seedbox process"
)]
fn main() {
    // Native PV machines: ARM64 on macOS/HVF or Linux/KVM, x86-64 on Linux/KVM.
    println!("cargo:rustc-check-cfg=cfg(seedbox_pv)");
    // Set only by the QEMU disk static library build (qemu/seedbox_disk.rs).
    println!("cargo:rustc-check-cfg=cfg(seedbox_disk_abi)");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(
        (arch.as_str(), os.as_str()),
        ("aarch64", "linux" | "macos") | ("x86_64", "linux")
    ) {
        println!("cargo:rustc-cfg=seedbox_pv");
    }
    println!("cargo:rerun-if-changed=src/machine/pv/worker/hvf_vector.c");
    // Off-Mac cross checks (`cargo check --target aarch64-apple-darwin`) have
    // no Apple SDK; they type-check the Rust and never link, so the HVF vector
    // object is only built on a macOS host.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
        && !cfg!(target_os = "macos")
    {
        println!("cargo:warning=no Apple SDK on this host: skipping hvf_vector.c; the result cannot link");
    } else if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        let output = std::process::Command::new("/usr/bin/xcrun")
            .args(["--sdk", "macosx", "--find", "clang"])
            .output()
            .expect("Apple SDK clang; install Xcode Command Line Tools");
        assert!(output.status.success(), "Apple SDK clang unavailable");
        cc::Build::new()
            .compiler(
                String::from_utf8(output.stdout)
                    .expect("Apple clang path")
                    .trim(),
            )
            .file("src/machine/pv/worker/hvf_vector.c")
            .compile("seedbox_pv_vector");
    }
}
