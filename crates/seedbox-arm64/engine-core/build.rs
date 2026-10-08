fn main() {
    // Watch the directory recursively, including changed or newly added modules.
    // rustc dep-info canonicalizes symlinked files outside this package.
    println!("cargo::rerun-if-changed=../../seedbox-engine-core/src");
}
