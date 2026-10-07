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

/// Historical ABI-1 acceptance: run at 66cf845, before the ABI-2 page change.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
#[ignore = "historical ABI-1 test: requires checkout 66cf845, signed CLI, HVF and old inputs"]
fn hvf_old_fixture_regression() {
    use seedbox_arm64::witness::{hash, Report};
    let input = |key| std::path::PathBuf::from(std::env::var_os(key).expect(key));
    let kernel = input("SEEDBOX_ARM64_KERNEL");
    let initrd = input("SEEDBOX_ARM64_INITRD");
    let output = input("SEEDBOX_ARM64_REPORT_DIR");
    std::fs::create_dir_all(&output).unwrap();
    assert_eq!(
        hash(&std::fs::read(&kernel).unwrap()),
        "7ea8fa2858ef33d05da559e9de5e5a9329c6d397e5b03fd7f4fcd3ebb75d9ff1"
    );
    assert_eq!(
        hash(&std::fs::read(&initrd).unwrap()),
        "9b952991625d7603e2db03e5ccdb87f78f1f7a16e6c664062505b7ddc75d8649"
    );
    for (seed, witness, trace) in [
        (
            "1",
            "b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7",
            "e91d56cedf3c759bebba911a8300f89e4b1b80dfd337e996f37e6c0d8879994a",
        ),
        (
            "2",
            "3477c8e474cbe111646f5a6aa03acadffefc5bb2995212bcd2aafec648e6dabe",
            "3df66d85acd202aa54e129cd7c46cc8f6978d5653ecb7ea0a51fcef8a1c5e5fa",
        ),
    ] {
        for repeat in 1..=2 {
            let path = output.join(format!("seed{seed}-{repeat}.json"));
            let out = Command::new(env!("CARGO_BIN_EXE_seedbox-arm64"))
                .args(["run", "--backend", "hvf", "--seed", seed, "--kernel"])
                .arg(&kernel)
                .arg("--initrd")
                .arg(&initrd)
                .arg("--witness")
                .arg(&path)
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(out.stdout, format!("WITNESS: {witness}\n").as_bytes());
            let report: Report = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(report.backend, "hvf");
            assert_eq!(report.witness, witness);
            assert_eq!(report.trace_hash, trace);
            assert_eq!(report.exits, 2291);
            assert_eq!(report.virtual_ns, 1_010_051_041);
            eprintln!("old fixture seed {seed} repeat {repeat}: {witness}");
        }
    }
}
