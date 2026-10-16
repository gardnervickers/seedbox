//! Process isolation for tests that write and execute temporary programs.
//!
//! A concurrent fork can inherit a writable executable descriptor until exec.
//! Run these scenarios alone in a child test harness so unrelated unit tests
//! cannot inherit their descriptors. No production APIs or global locks needed.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: spawns here never race a Seedbox process writing an executable"
)]

/// Return true only in the child running this exact scenario. The parent
/// checks the child's result and returns false without creating any fixtures.
pub(crate) fn isolated(qualified_name: &str) -> bool {
    const CHILD: &str = "SEEDBOX_ISOLATED_TEST";
    // libtest names omit the crate prefix supplied by module_path!().
    let (_, name) = qualified_name.split_once("::").unwrap();
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed"),
        "isolated test {name} failed or did not run: {}\n{stdout}\n{stderr}",
        output.status
    );
    false
}
