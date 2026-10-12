//! Drive two native ARM PV Linux guests with seeded sleeps of one to four seconds.
//! Run inside `nix develop` with SEEDBOX_PV_WORKER and SEEDBOX_PV_KERNEL set.
//! The worker is the standalone seedbox-arm64 executable, signed for HVF on macOS.

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
#[path = "support/pv_init.rs"]
mod pv_init;
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
mod support;

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine::{
        driver::{Driver, StepOutcome},
        machine::{pv::PVMachine, ExitReason, StopReason, Timestamp},
    };
    use std::{fs, path::PathBuf};

    fn input(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{name} is missing; set it to the prepared PV input").into())
    }
    let executable = input("SEEDBOX_PV_WORKER")?;
    let kernel = fs::read(input("SEEDBOX_PV_KERNEL")?)?;
    let guests = support::sleeping_guests();
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        std::time::Duration::from_millis(1),
        2,
        || Ok(()),
        |clock| {
            guests
                .iter()
                .map(|(_, seconds)| {
                    PVMachine::new(
                        &executable,
                        kernel.clone(),
                        pv_init::sleeping_initrd(*seconds),
                        support::SEED,
                        clock.clone(),
                    )
                })
                .collect()
        },
    )?;
    let ids: Vec<_> =
        driver.with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect()))?;
    println!("[seed={}, sleep range=1..=4 seconds]", support::SEED);
    for (name, sleep) in guests {
        println!("[{name} sleeping for {sleep} seconds]");
    }
    let mut now = Timestamp::from_nanos(0);
    let mut exited = [false; 2];
    const QUANTUM_NS: u64 = 1_000_000; // One millisecond of simulated execution.
    loop {
        let outcome = driver.step(Timestamp::from_nanos(now.as_nanos() + QUANTUM_NS))?;
        now = match outcome {
            StepOutcome::BarrierReached { at } | StepOutcome::Finished { at } => at,
            StepOutcome::Blocked { .. } => return Err("PV guests blocked without a timer".into()),
            StepOutcome::Waiting { .. }
            | StepOutcome::BeforeAction { .. }
            | StepOutcome::Stopped { .. } => {
                return Err("unexpected playback control in example".into())
            }
        };
        let stops: Vec<_> =
            driver.with_barrier(|barrier| ids.iter().map(|&id| barrier.stopped(id)).collect())?;
        for (index, stopped) in stops.into_iter().enumerate() {
            if let StopReason::Exited { reached, reason } = stopped {
                let (name, seconds) = guests[index];
                if reason != ExitReason::Shutdown {
                    return Err(format!("{name} exited: {reason:?}").into());
                }
                if reached.as_nanos() < u64::from(seconds) * 1_000_000_000 {
                    return Err(format!("{name} shut down before its sleep elapsed").into());
                }
                if !exited[index] {
                    println!("[{name} shut down at {} ns]", reached.as_nanos());
                    exited[index] = true;
                }
            }
        }
        if matches!(outcome, StepOutcome::Finished { .. }) {
            break;
        }
    }
    Ok(())
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))))]
fn main() {
    eprintln!("the PV example requires ARM64 with HVF on macOS or KVM on Linux");
}
