fn main() {
    println!("cargo:rustc-check-cfg=cfg(seedbox_pv)");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(
        (arch.as_str(), os.as_str()),
        ("aarch64", "linux" | "macos") | ("x86_64", "linux")
    ) {
        println!("cargo:rustc-cfg=seedbox_pv");
    }
}
