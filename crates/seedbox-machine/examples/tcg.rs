//! Boot Linux, exercise a timer wakeup in userspace, and shut down.
//! Run inside `nix develop`: cargo run -p seedbox-machine --example tcg

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine::machine::{
        tcg::{Boot, Manifest, TcgMachine},
        ExitReason, Machine, StopReason, Timestamp,
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
    let mut machine = TcgMachine::new(&input("SEEDBOX_TCG_QEMU")?, manifest)?;
    let mut now = 0;
    let mut printed = 0;
    let mut console = io::stdout().lock();
    const QUANTUM_NS: u64 = 1_000_000; // One millisecond of simulated execution.

    // No host sleeps: the driver runs budgets and jumps only to reported timers.
    loop {
        let upto = Timestamp::from_nanos(now + QUANTUM_NS);
        let stopped = machine.run(&mut (), upto)?;
        let output = machine.console()?;
        console.write_all(&output[printed..])?;
        console.flush()?;
        printed = output.len();
        match stopped {
            StopReason::BoundaryReached => now = upto.as_nanos(),
            StopReason::Blocked {
                deadline: Some(at), ..
            } => {
                machine.wake(at)?;
                now = at.as_nanos();
            }
            StopReason::Blocked { deadline: None, .. } => {
                return Err("guest is waiting for external input with no timer".into());
            }
            StopReason::Exited {
                reason: ExitReason::Shutdown,
                ..
            } => break,
            StopReason::Exited { reason, .. } => {
                return Err(format!("guest exited: {reason:?}").into())
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("the TCG reference backend requires Unix sockets (macOS or Linux)");
}
