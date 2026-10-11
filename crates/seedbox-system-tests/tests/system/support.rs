use seedbox_system_tests::KernelLease;

pub fn kernel() -> KernelLease {
    // Sudo and the original user must use the same lease namespace. The runner
    // creates it as the original user before any privileged execution.
    let directory = KernelLease::directory();
    eprintln!("waiting for hypervisor lease: {}", directory.display());
    KernelLease::acquire(&directory).expect("acquire system-test hypervisor lease")
}

/// Bounded client operations; the owning server fixture performs execution cleanup.
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
pub fn status(
    command: &mut std::process::Command,
    limit: std::time::Duration,
) -> std::io::Result<std::process::ExitStatus> {
    let mut child = command.spawn()?;
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "system-test client exceeded its deadline",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
