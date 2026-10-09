fn main() {
    // Cargo supplies this build-only input through the checkout's config. Unlike
    // Cargo's built-in variables, it invalidates warm cross-checkout fingerprints.
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_ROOT");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo package directory");
    let expected = std::fs::canonicalize(std::path::Path::new(&manifest).join("../../.."))
        .expect("shared source checkout");
    let configured =
        std::env::var_os("SEEDBOX_BUILD_ROOT").and_then(|path| std::fs::canonicalize(path).ok());
    if configured.as_ref() != Some(&expected) {
        // An older checkout can reuse this build-script executable without our
        // config input. A missing watched file forces a fresh source check on
        // every invocation, including repeated outside-root builds.
        let output = std::env::var_os("OUT_DIR").expect("Cargo build output directory");
        let absent = std::path::Path::new(&output).join("seedbox-unconfigured-source-cache");
        println!("cargo::rerun-if-changed={}", absent.display());
    }
    // Resolve from the runtime package directory, never the compiled script's
    // original checkout or an unrelated inherited build-root value.
    let source = std::path::Path::new(&manifest).join("../../seedbox-control/src");
    println!("cargo::rerun-if-changed={}", source.display());
}
