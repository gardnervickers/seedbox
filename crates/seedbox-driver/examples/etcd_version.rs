//! One-guest etcd compatibility smoke through the public Machine/Driver API.
//! Set SEEDBOX_ETCD_BACKEND=pv|tcg, SEEDBOX_ETCD_BINARY to a static guest
//! executable, and the usual SEEDBOX_{PV,TCG}_{ASSETS,WORKER,QEMU} inputs.
//! Both backends use the normal x86-64 etcd binary without runtime overrides.
//! This checks startup only, not cluster operation or determinism.
use seedbox_driver::driver::{Driver, EventKind, NetworkConfig, StepOutcome};
use seedbox_machine::{
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    machine::{ExitReason, Machine, Timestamp},
    pv::{Response, Stream},
    rng::RngStream,
};
use std::{fmt::Debug, io::Write, path::PathBuf, time::Duration};

const EPOCH: u32 = 946_684_800;
const LIMIT_NS: u64 = 30_000_000_000;

// A caller-owned deterministic byte stream; no changes to the public RNG API.
struct Bytes(u64);
impl RngStream for Bytes {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            *byte = self.0 as u8;
        }
    }
}

fn input(name: &str) -> Result<PathBuf, String> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is required"))
}

fn definition() -> Result<MachineDefinition, Box<dyn std::error::Error>> {
    let program = std::fs::read(input("SEEDBOX_ETCD_BINARY")?)?;
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(program.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, "etcd", &program[..])?;
    Ok(MachineDefinition {
        name: "oracle".into(),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(archive.into_inner()?)?,
            launch: Launch {
                argv: vec!["/etcd".into(), "--version".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    })
}

fn run<M, F>(definition: MachineDefinition, factory: F) -> Result<(), String>
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
    F: FnMut(&MachineDefinition) -> Result<M, M::Error> + Send + 'static,
{
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        Duration::from_millis(10),
        1,
        Bytes(7),
        || Ok(()),
        definition,
        Vec::new(),
        Vec::new(),
        NetworkConfig::default(),
        factory,
    )
    .map_err(|error| format!("construct guest: {error:?}"))?;
    let started = std::time::Instant::now();
    let mut now = 0;
    let mut stdout = Vec::new();
    while now < LIMIT_NS && started.elapsed() < Duration::from_secs(300) {
        let outcome = driver
            .step(Timestamp::from_nanos((now + 10_000_000).min(LIMIT_NS)))
            .map_err(|error| format!("run guest at {now} ns: {error:?}"))?;
        for event in driver.drain_observations().events {
            if let EventKind::Output(output) = event.kind {
                match output.payload {
                    Response::ApplicationOutput { stream, bytes, .. } => {
                        if stream == Stream::Stdout {
                            stdout.extend_from_slice(&bytes);
                            std::io::stdout()
                                .write_all(&bytes)
                                .map_err(|e| e.to_string())?;
                        } else {
                            std::io::stderr()
                                .write_all(&bytes)
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    Response::BootConsole { bytes } => {
                        std::io::stderr()
                            .write_all(&bytes)
                            .map_err(|e| e.to_string())?;
                    }
                    _ => {}
                }
            }
        }
        match outcome {
            StepOutcome::BarrierReached { at } => now = at.as_nanos(),
            StepOutcome::Finished { at, reason } => {
                if reason != (ExitReason::ProgramExit { code: 0 }) {
                    return Err(format!("etcd exited unsuccessfully at {at:?}: {reason:?}"));
                }
                if !String::from_utf8_lossy(&stdout).contains("etcd Version:") {
                    return Err("successful exit did not print etcd's version".into());
                }
                eprintln!("etcd version smoke passed at {} ns", at.as_nanos());
                return Ok(());
            }
            other => {
                return Err(format!(
                    "guest stopped before version completion: {other:?}"
                ))
            }
        }
    }
    Err(format!("etcd version smoke exceeded its bound at {now} ns"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let definition = definition()?;
    match std::env::var("SEEDBOX_ETCD_BACKEND")?.as_str() {
        #[cfg(seedbox_pv)]
        "pv" => {
            let mut factory = seedbox_machine_pv::machine::pv::Factory::new(
                &input("SEEDBOX_PV_WORKER")?,
                &input("SEEDBOX_PV_ASSETS")?,
                EPOCH,
            )?;
            run(definition, move |definition| factory.create(definition))?;
        }
        #[cfg(unix)]
        "tcg" => {
            let mut factory = seedbox_machine_tcg::machine::tcg::Factory::new(
                &input("SEEDBOX_TCG_QEMU")?,
                &input("SEEDBOX_TCG_ASSETS")?,
                EPOCH,
            )?;
            run(definition, move |definition| factory.create(definition))?;
        }
        other => return Err(format!("unsupported etcd smoke backend: {other}").into()),
    }
    Ok(())
}
