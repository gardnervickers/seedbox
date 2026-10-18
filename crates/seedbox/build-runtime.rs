//! Signing belongs to the executable that hosts the native worker.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        println!("cargo::rustc-link-arg-bin=seedbox=--seedbox-sign-hvf");
    }
}
