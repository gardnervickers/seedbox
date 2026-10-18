//! Only explicitly selected Darwin executables receive the HVF entitlement.
#![cfg(unix)]
mod support;

use std::{fs, process::Command};
use support::{python, success};

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "requires Apple's linker and codesign"
)]
fn only_explicitly_selected_executables_receive_hvf_entitlement() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("main.c");
    fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    for selected in [false, true] {
        let output = directory
            .path()
            .join(if selected { "seedbox" } else { "dependency" });
        let mut command = python("linker.py");
        command.arg(&source).arg("-o").arg(&output);
        if selected {
            command.arg("--seedbox-sign-hvf");
        }
        success(&mut command, 120);
        let result = success(
            Command::new("/usr/bin/codesign")
                .args(["--display", "--entitlements", ":-"])
                .arg(output),
            30,
        );
        let text = [result.stdout, result.stderr].concat();
        assert_eq!(
            text.windows(b"com.apple.security.hypervisor".len())
                .any(|b| b == b"com.apple.security.hypervisor"),
            selected,
            "selected={selected}: {}",
            String::from_utf8_lossy(&text)
        );
    }
}
