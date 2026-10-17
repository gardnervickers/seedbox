//! Link TigerBeetle's C client (`libtb_client.a`) when it is present.
//!
//! `seedbox-jepsen fetch-tb-client` (and `build-oracle`, for the guest) places
//! the pinned release library at `target/native/tb_client/ARCH-OS/` under this
//! crate. With it, the build sets `cfg(tb_client)` and the TigerBeetle client
//! talks to servers; without it the client reports that it is unavailable, so
//! the host tools and checkers still build everywhere.
use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rustc-check-cfg=cfg(tb_client)");
    println!("cargo:rustc-check-cfg=cfg(tb_client_0_16_17)");
    println!("cargo:rustc-check-cfg=cfg(tb_client_0_17_9)");
    println!("cargo:rerun-if-env-changed=TB_CLIENT_LIB_DIR");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let dir = env::var_os("TB_CLIENT_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
                .join("target/native/tb_client")
                .join(format!("{arch}-{os}"))
        });
    let library = dir.join("libtb_client.a");
    println!("cargo:rerun-if-changed={}", library.display());
    if library.is_file() {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=tb_client");
        println!("cargo:rustc-cfg=tb_client");
        // 0.16.17's client, its exports renamed tb17_client_* (see tools.rs).
        let newer = dir.join("libtb_client_0_16_17.a");
        println!("cargo:rerun-if-changed={}", newer.display());
        if newer.is_file() {
            println!("cargo:rustc-link-lib=static=tb_client_0_16_17");
            println!("cargo:rustc-cfg=tb_client_0_16_17");
        }
        // 0.17.9's client, its exports renamed tb179_client_*.
        let current = dir.join("libtb_client_0_17_9.a");
        println!("cargo:rerun-if-changed={}", current.display());
        if current.is_file() {
            println!("cargo:rustc-link-lib=static=tb_client_0_17_9");
            println!("cargo:rustc-cfg=tb_client_0_17_9");
        }
    }
}
