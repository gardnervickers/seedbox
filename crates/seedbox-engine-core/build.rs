// Source-cache watcher for seedbox-engine-core, also named as the `build`
// script of seedbox-engine-protocol and seedbox-spec. Each package resolves
// its own `src/lib.rs`, so the checkout check applies to every user.
fn main() {
    // Cargo supplies this build-only input through the checkout's config. Unlike
    // Cargo's built-in variables, it invalidates warm cross-checkout fingerprints.
    println!("cargo::rerun-if-env-changed=SEEDBOX_BUILD_ROOT");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo package directory");
    // Resolve from the runtime package directory, never the compiled script's
    // original checkout or an unrelated inherited build-root value.
    let library = std::fs::canonicalize(std::path::Path::new(&manifest).join("src/lib.rs"))
        .expect("shared source library");
    let source = library.parent().expect("shared source directory");
    // The canonical source is crates/<package>/src in the checkout.
    let expected = source.ancestors().nth(3).expect("shared source checkout");
    let configured =
        std::env::var_os("SEEDBOX_BUILD_ROOT").and_then(|path| std::fs::canonicalize(path).ok());
    if configured.as_deref() != Some(expected) {
        // An older checkout can reuse this build-script executable without our
        // config input. A missing watched file forces a fresh source check on
        // every invocation, including repeated outside-root builds.
        let output = std::env::var_os("OUT_DIR").expect("Cargo build output directory");
        let absent = std::path::Path::new(&output).join("seedbox-unconfigured-source-cache");
        println!("cargo::rerun-if-changed={}", absent.display());
    }
    println!("cargo::rerun-if-changed={}", source.display());
}
