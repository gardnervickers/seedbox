use std::process::Command;
#[test]
fn help_and_usage_do_not_open_hypervisor() {
    let p = env!("CARGO_BIN_EXE_seedbox-arm64");
    let out = Command::new(p).args(["run", "--help"]).output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    for flag in [
        "--kernel",
        "--initrd",
        "--seed",
        "--backend",
        "--witness",
        "--ram-pages",
    ] {
        assert!(help.contains(flag));
    }
    let out = Command::new(p)
        .args([
            "run",
            "--kernel",
            "/missing-arm64-Image",
            "--initrd",
            "/missing.cpio",
            "--seed",
            "x",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--seed"));
}
#[test]
fn missing_input_is_contextual() {
    let out = Command::new(env!("CARGO_BIN_EXE_seedbox-arm64"))
        .args([
            "run",
            "--kernel",
            "/missing-arm64-Image",
            "--initrd",
            "/missing.cpio",
            "--seed",
            "1",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("ERROR: --kernel /missing-arm64-Image:")
    );
}

#[test]
fn ram_pages_refuses_existing_file_before_opening_hypervisor() {
    let path = std::env::temp_dir().join(format!("seedbox-pages-{}", std::process::id()));
    std::fs::write(&path, b"preserve me").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_seedbox-arm64"))
        .args([
            "run",
            "--kernel",
            "/dev/null",
            "--initrd",
            "/dev/null",
            "--seed",
            "1",
            "--ram-pages",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--ram-pages"));
    assert_eq!(std::fs::read(&path).unwrap(), b"preserve me");
    std::fs::remove_file(path).unwrap();
}
