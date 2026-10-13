//! Drive two standard RISC-V Linux guests with seeded sleeps of one to four seconds.
//! Supply the backend guest asset directory and patched QEMU.

#[cfg(unix)]
mod support;
#[cfg(unix)]
#[path = "support/tcg_program.rs"]
mod tcg_program;

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine::{
        definition::{
            FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources,
        },
        driver::{Driver, EventKind, StepOutcome},
        machine::{tcg::Factory, ExitReason, Timestamp},
    };
    use std::path::PathBuf;

    fn input(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        std::env::var_os(name).map(PathBuf::from).ok_or_else(|| {
            let fix = match name {
                "SEEDBOX_TCG_QEMU" => "set this to the patched QEMU executable",
                "SEEDBOX_TCG_ASSETS" => "set this to the backend guest asset directory",
                _ => "set this to the required TCG input",
            };
            format!("{name} is missing; {fix}").into()
        })
    }
    let executable = input("SEEDBOX_TCG_QEMU")?;
    let mut backend = Factory::new(&executable, &input("SEEDBOX_TCG_ASSETS")?)?;
    let guests = support::sleeping_guests();
    fn definition(
        name: String,
        seconds: u32,
    ) -> Result<MachineDefinition, Box<dyn std::error::Error>> {
        let program = tcg_program::sleeping_program(seconds);
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(program.len() as u64);
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(&mut header, "application", &program[..])?;
        Ok(MachineDefinition {
            name,
            resources: Resources {
                memory_bytes: 128 << 20,
            },
            application: PreparedApplication {
                filesystem: FilesystemArchive::new(tar.into_inner()?)?,
                launch: Launch {
                    argv: vec!["/application".into()],
                    environment: Default::default(),
                    working_directory: "/".into(),
                    user: "0:0".into(),
                },
            },
        })
    }
    // The oracle outlives both ordinary sleepers and ends this demonstration.
    let oracle = definition("oracle".into(), 5)?;
    let machines = guests
        .iter()
        .map(|(name, seconds)| definition((*name).into(), *seconds))
        .collect::<Result<Vec<_>, _>>()?;
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        std::time::Duration::from_millis(1),
        2,
        support::driver_rng(),
        || Ok(()),
        oracle,
        machines,
        move |definition| backend.create(definition),
    )?;
    let ids: Vec<_> = driver
        .with_barrier(|barrier| Ok(barrier.machines().skip(1).map(|(id, _)| id).collect()))?;
    println!("[seed={}, sleep range=1..=4 seconds]", support::SEED);
    for (name, sleep) in &guests {
        println!("[{name} sleeping for {sleep} seconds]");
    }
    let mut now = Timestamp::from_nanos(0);
    let mut exited = [false; 2];
    const QUANTUM_NS: u64 = 1_000_000; // One millisecond of simulated execution.
    loop {
        let outcome = driver.step(Timestamp::from_nanos(now.as_nanos() + QUANTUM_NS))?;
        now = match outcome {
            StepOutcome::BarrierReached { at } | StepOutcome::Finished { at, .. } => at,
            StepOutcome::Blocked { .. } => return Err("TCG guests blocked without a timer".into()),
            StepOutcome::Waiting { .. }
            | StepOutcome::BeforeAction { .. }
            | StepOutcome::Stopped { .. } => {
                return Err("unexpected playback control in example".into());
            }
        };
        let outputs = driver
            .drain_events()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Output(output) => Some(output),
                _ => None,
            })
            .collect::<Vec<_>>();
        for output in outputs {
            if let seedbox_machine::pv::Response::ApplicationOutcome { code } = output.payload {
                if let Some(index) = ids.iter().position(|&id| id == output.machine) {
                    let (name, seconds) = &guests[index];
                    if code != 0 || output.at.as_nanos() < u64::from(*seconds) * 1_000_000_000 {
                        return Err(format!(
                            "{name} unexpected application outcome: {code} at {} ns",
                            output.at.as_nanos()
                        )
                        .into());
                    }
                    if exited[index] {
                        return Err(format!("{name} duplicate application outcome").into());
                    }
                    println!("[{name} application exited at {} ns]", output.at.as_nanos());
                    exited[index] = true;
                }
            }
        }
        if let StepOutcome::Finished { reason, .. } = outcome {
            if reason != (ExitReason::ProgramExit { code: 0 }) || exited != [true; 2] {
                return Err(format!(
                    "oracle finished before successful ordinary outcomes: {reason:?}"
                )
                .into());
            }
            break;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("the TCG example requires Unix sockets (macOS or Linux)");
}
