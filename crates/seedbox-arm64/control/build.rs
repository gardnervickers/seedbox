fn main() {
    // rustc's dep-info canonicalizes this shared source outside the package.
    // An explicit package-relative watcher also follows a new worktree when
    // Cargo reuses its target directory and unchanged package metadata.
    println!("cargo::rerun-if-changed=../../seedbox-control/src/lib.rs");
}
