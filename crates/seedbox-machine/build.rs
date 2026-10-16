fn main() {
    println!("cargo:rustc-check-cfg=cfg(seedbox_disk_abi)");
}
