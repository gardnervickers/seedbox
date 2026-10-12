//! Drive two Linux guests on one clock and observe their different sleep times.
//! Run inside `nix develop`: cargo run -p seedbox-machine --example tcg

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine::driver::{Driver, StepOutcome};
    use seedbox_machine::machine::{
        tcg::{Boot, Manifest, TcgMachine},
        ExitReason, StopReason, Timestamp,
    };
    use std::{
        fs,
        io::{self, Write},
        path::PathBuf,
    };

    fn input(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{name} is missing; run this example inside nix develop").into())
    }
    let assets = input("SEEDBOX_TCG_LINUX")?;
    let manifest = Manifest {
        memory_mib: 256,
        boot: Boot::Linux {
            kernel: fs::read(assets.join("kernel"))?,
            initramfs: fs::read(assets.join("initramfs"))?,
            firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")?)?,
            command_line: "console=ttyS0 earlycon=sbi rdinit=/init nokaslr".into(),
        },
    };
    // Put the long sleeper first so insertion order cannot explain wake order.
    let guests = [("slow", "2"), ("fast", "1")];
    let qemu = input("SEEDBOX_TCG_QEMU")?;
    let mut driver = Driver::new(Timestamp::from_nanos(0), (), |clock| {
        guests
            .iter()
            .map(|(name, sleep)| {
                let mut manifest = manifest.clone();
                if let Boot::Linux { command_line, .. } = &mut manifest.boot {
                    command_line.push_str(&format!(" seedbox_name={name} seedbox_sleep={sleep}"));
                }
                TcgMachine::new(&qemu, manifest, clock.clone())
            })
            .collect()
    })?;
    let ids: Vec<_> =
        driver.with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect()))?;
    let mut now = Timestamp::from_nanos(0);
    let mut printed = [0; 2];
    let mut exited = [false; 2];
    let mut console = io::stdout().lock();
    const QUANTUM_NS: u64 = 1_000_000; // One millisecond of simulated execution.

    // Each round brings both guests to a common boundary or a blocked/exit state.
    loop {
        let upto = Timestamp::from_nanos(now.as_nanos() + QUANTUM_NS);
        let outcome = driver.step(upto)?;
        now = match outcome {
            StepOutcome::BarrierReached { at }
            | StepOutcome::Blocked { at }
            | StepOutcome::Finished { at } => at,
        };
        let observations: Vec<_> = driver.with_barrier(|barrier| {
            ids.iter()
                .map(|&id| {
                    let output = barrier
                        .machine(id)?
                        .console()
                        .map_err(|source| seedbox_machine::driver::Error::Machine { id, source })?;
                    Ok((output, barrier.stopped(id)?))
                })
                .collect()
        })?;
        for (index, (name, _)) in guests.iter().enumerate() {
            let (output, stopped) = &observations[index];
            if output.len() > printed[index] {
                writeln!(console, "[{name}, output through {} ns]", now.as_nanos())?;
                console.write_all(&output[printed[index]..])?;
                console.flush()?;
                printed[index] = output.len();
            }
            match stopped {
                StopReason::BoundaryReached
                | StopReason::Blocked {
                    deadline: Some(_), ..
                } => {}
                StopReason::Blocked { deadline: None, .. } => {
                    return Err(
                        format!("{name} is waiting for external input with no timer").into(),
                    );
                }
                StopReason::Exited {
                    reason: ExitReason::Shutdown,
                    reached,
                } => {
                    if !exited[index] {
                        writeln!(console, "[{name} shut down at {} ns]", reached.as_nanos())?;
                        exited[index] = true;
                    }
                }
                StopReason::Exited { reason, .. } => {
                    return Err(format!("{name} exited: {reason:?}").into())
                }
            }
        }
        if matches!(outcome, StepOutcome::Finished { .. }) {
            break;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("the TCG reference backend requires Unix sockets (macOS or Linux)");
}
