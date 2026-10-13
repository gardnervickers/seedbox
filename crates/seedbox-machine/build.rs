fn main() {
    println!("cargo:rerun-if-changed=src/machine/pv/worker/hvf_vector.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
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
