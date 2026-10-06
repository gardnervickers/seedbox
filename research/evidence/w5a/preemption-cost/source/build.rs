use std::{env, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=fixtures/guest.asm");
    println!("cargo:rerun-if-changed=fixtures/preempt.asm");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    assert!(
        Command::new("nasm")
            .args(["-f", "bin", "fixtures/guest.asm", "-o"])
            .arg(out.join("guest.bin"))
            .status()
            .expect("nasm in pinned shell")
            .success()
    );
    assert!(Command::new("nasm").args(["-f","bin","fixtures/preempt.asm","-o"]).arg(out.join("preempt.bin")).status().expect("nasm").success());
}
