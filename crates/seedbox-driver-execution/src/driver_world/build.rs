//! Construct stopped native Drivers from explicit retained service inputs.
#[cfg(test)]
mod policy_tests;
use super::{
    checkpoint,
    native::{Factory, Native, Snapshot},
    worker::{Constructed, Worker},
};
use crate::{Error, Result};
use seedbox_driver::driver::{Driver, LinkPolicy, NetworkConfig, NetworkPolicy, NetworkSetup};
use seedbox_engine_protocol::driver_world::{self as wire, Artifact, Inventory, State};
use seedbox_machine::{
    definition::{self, FilesystemArchive, MachineDefinition},
    machine::Timestamp,
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufWriter, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
fn bad(message: impl Into<String>) -> Error {
    Error::Integrity(message.into())
}

pub(super) fn open(request: &wire::Request) -> Result<Worker<Native>> {
    if request.machine_workers == 0 {
        return Err(bad("Driver worker count must be positive"));
    }
    super::policy::decode_optional(request.prepared.inventory.backend_policy()).map_err(bad)?;
    super::controller::resolve(
        &request.prepared.inventory,
        &request.prepared.oracle.name,
        request.prepared.network.latency_ns,
    )?;
    super::policy::admit(&request.prepared.inventory).map_err(bad)?;
    fs::create_dir_all(request.output.join(super::NATIVE_SCRATCH))?;
    if let Some(path) = &request.resume_checkpoint {
        restore(request, path)
    } else {
        fresh(request)
    }
}
fn fresh(request: &wire::Request) -> Result<Worker<Native>> {
    let program = program(request)?;
    let assets = materialize_factory(
        &request.prepared.inventory,
        &request.output,
        request.runtime_binding.as_ref(),
    )?;
    let mut factory = Factory::load(
        &assets.inventory,
        &assets.executable,
        &assets.guest_assets,
        assets.inputs.clone(),
        &request.output.join(super::NATIVE_SCRATCH),
    )
    .map_err(bad)?;
    let oracle = definition(&request.prepared.oracle)?;
    let machines = request
        .prepared
        .machines
        .iter()
        .map(definition)
        .collect::<Result<Vec<_>>>()?;
    let spawnable = request
        .prepared
        .spawnable
        .iter()
        .map(definition)
        .collect::<Result<Vec<_>>>()?;
    let mut initial: Vec<_> = std::iter::once(oracle.name.clone())
        .chain(machines.iter().map(|machine| machine.name.clone()))
        .collect();
    initial.sort();
    let constructed: Constructed = Arc::new(Mutex::new(VecDeque::new()));
    let ledger = constructed.clone();
    let network = NetworkConfig::new(
        Duration::from_nanos(request.prepared.network.latency_ns),
        Duration::from_nanos(request.prepared.network.jitter_ns),
        request.prepared.network.loss_ppm,
    )
    .map_err(|e| bad(e.to_string()))?;
    let network = NetworkSetup::new(
        network,
        request.prepared.network.bandwidth_bps,
        request.prepared.network.queue_bound_ns,
        request
            .prepared
            .network
            .links
            .iter()
            .map(|link| LinkPolicy {
                from: link.from.clone(),
                to: link.to.clone(),
                policy: NetworkPolicy {
                    latency_ns: link.latency_ns,
                    jitter_ns: link.jitter_ns,
                    loss_ppm: link.loss_ppm,
                    bandwidth_bps: link.bandwidth_bps,
                    queue_bound_ns: link.queue_bound_ns,
                },
            })
            .collect(),
    )
    .map_err(bad)?;
    let network = super::controller::resolve(
        &request.prepared.inventory,
        &request.prepared.oracle.name,
        request.prepared.network.latency_ns,
    )?
    .configure(network)?;
    let mut driver = Driver::new_seeded_with_network(
        Timestamp::from_nanos(0),
        Duration::from_nanos(request.prepared.quantum_ns),
        request.machine_workers,
        request.seed,
        || Ok(()),
        oracle,
        machines,
        spawnable,
        network,
        move |definition| {
            let machine = factory.create(definition)?;
            ledger
                .lock()
                .map_err(|_| "Driver construction ledger poisoned".to_owned())?
                .push_back(definition.name.clone());
            Ok(machine)
        },
    )
    .map_err(|e| bad(e.to_string()))?;
    if request.start_paused {
        driver
            .with_barrier(|barrier| barrier.set_rate(seedbox_driver::driver::PlaybackRate::Paused))
            .map_err(|e| bad(e.to_string()))?;
    }
    let state = State {
        world: wire::World {
            seed: request.seed,
            machine_names: vec![],
            input_program: wire::Program {
                inputs: program,
                ..Default::default()
            },
            intervention_next: 0,
            readiness: wire::Readiness {
                initial,
                seen: vec![],
                latched: false,
            },
            publication: Default::default(),
            position: Default::default(),
        },
    };
    let mut worker = Worker::new(
        driver,
        state,
        assets.inventory.clone(),
        request.output.clone(),
        BTreeMap::new(),
        constructed,
        request.observations.as_deref(),
        encode,
    )?;
    worker.flush()?;
    Ok(worker)
}
pub(super) fn validate_policy_binding(expected: &Inventory, captured: &Inventory) -> Result<()> {
    super::policy::decode_optional(expected.backend_policy()).map_err(bad)?;
    super::policy::decode_optional(captured.backend_policy()).map_err(bad)?;
    if std::mem::discriminant(expected) != std::mem::discriminant(captured)
        || expected.backend_policy() != captured.backend_policy()
    {
        return Err(bad("Driver checkpoint backend policy differs from requested factory authority; None is not Some(default), and restore never reconfigures or reseeds a saved machine"));
    }
    Ok(())
}

fn validate_checkpoint_quantum<S>(
    checkpoint: &seedbox_driver::driver::Checkpoint<S>,
    quantum_ns: u64,
) -> Result<()> {
    if checkpoint.quantum() != Duration::from_nanos(quantum_ns) {
        return Err(bad(
            "Driver checkpoint quantum differs from the simulation definition; start fresh to change quantum",
        ));
    }
    Ok(())
}

fn restore(request: &wire::Request, path: &Path) -> Result<Worker<Native>> {
    let mut envelope = checkpoint::read(path)?;
    if envelope.state.world.seed != request.seed {
        return Err(bad(
            "Driver checkpoint seed differs; restoration never reseeds a branch",
        ));
    }
    validate_policy_binding(&request.prepared.inventory, &envelope.inventory)?;
    let mut initial: Vec<_> = std::iter::once(request.prepared.oracle.name.clone())
        .chain(
            request
                .prepared
                .machines
                .iter()
                .map(|machine| machine.name.clone()),
        )
        .collect();
    let ordered_initial = initial.clone();
    initial.sort();
    if envelope.state.world.readiness.initial != initial
        || !envelope
            .state
            .world
            .machine_names
            .starts_with(&ordered_initial)
    {
        return Err(bad(
            "Driver checkpoint readiness/name inventory differs from prepared initial machines",
        ));
    }
    envelope
        .state
        .world
        .input_program
        .replace_pending(program(request)?)
        .map_err(bad)?;
    envelope
        .state
        .world
        .input_program
        .validate_at(envelope.state.world.position.simulated_time_ns)
        .map_err(bad)?;
    // Bind the codec5 controller/latency before extracting a factory runtime
    // or restoring even the first native machine. Saved queues stay in Driver.
    let retained = checkpoint::open(&envelope, driver_limits(envelope.driver.length)?)?;
    validate_checkpoint_quantum(&retained, request.prepared.quantum_ns)?;
    super::controller::resolve(
        &request.prepared.inventory,
        &request.prepared.oracle.name,
        request.prepared.network.latency_ns,
    )?
    .validate_saved(retained.controller_setup())?;
    let assets = materialize_factory(
        &envelope.inventory,
        &request.output,
        request.runtime_binding.as_ref(),
    )?;
    let mut factory = Factory::load(
        &assets.inventory,
        &assets.executable,
        &assets.guest_assets,
        assets.inputs.clone(),
        &request.output.join(super::NATIVE_SCRATCH),
    )
    .map_err(bad)?;
    let constructed: Constructed = Arc::new(Mutex::new(VecDeque::new()));
    let ledger = constructed.clone();
    // Only verified open descriptors accumulate here, not decoded RAM images.
    let retained = retained.try_map_snapshots(|file| Ok::<_, Error>(Snapshot::Retained(file)))?;
    let mut names = envelope.state.world.machine_names.iter();
    let native_limits = native_limits(
        envelope
            .machines
            .iter()
            .map(|body| body.length)
            .max()
            .ok_or_else(|| bad("Driver checkpoint has no native bodies"))?,
    )?;
    let mut driver = Driver::<Native>::from_checkpoint(
        &retained,
        request.machine_workers,
        || Ok(()),
        |snapshot| {
            let name = names
                .next()
                .ok_or_else(|| "Driver checkpoint name inventory is short".to_owned())?
                .clone();
            Native::restore(
                name,
                &assets.inventory,
                &assets.executable,
                snapshot,
                &native_limits,
                &request.output.join(super::NATIVE_SCRATCH),
            )
        },
        move |definition| {
            let machine = factory.create(definition)?;
            ledger
                .lock()
                .map_err(|_| "Driver construction ledger poisoned".to_owned())?
                .push_back(definition.name.clone());
            Ok(machine)
        },
    )
    .map_err(|e| bad(e.to_string()))?;
    if names.next().is_some() {
        return Err(bad("Driver checkpoint name inventory has unused entries"));
    }
    // Native snapshots decoded inside each callback above have already dropped.
    drop(retained);
    let map = driver
        .with_barrier(|barrier| {
            Ok(barrier
                .machines()
                .map(|(id, machine)| (id, machine.name().to_owned()))
                .collect::<BTreeMap<_, _>>())
        })
        .map_err(|e| bad(e.to_string()))?;
    // Playback is a request-owned host setting. Capturing while paused must not
    // make a new run (or replay) inherit an indefinitely paused worker.
    driver
        .with_barrier(|barrier| {
            barrier.set_rate(if request.start_paused {
                seedbox_driver::driver::PlaybackRate::Paused
            } else {
                seedbox_driver::driver::PlaybackRate::Unlimited
            })
        })
        .map_err(|e| bad(e.to_string()))?;
    let state = envelope.state;
    Worker::new(
        driver,
        state,
        assets.inventory,
        request.output.clone(),
        map,
        constructed,
        request.observations.as_deref(),
        encode,
    )
}
fn program(
    request: &wire::Request,
) -> Result<Vec<seedbox_engine_protocol::input_program::TimedInput>> {
    let mut inputs = request.program.clone();
    if let Some(history) = &request.replay_interventions {
        seedbox_engine_protocol::input_program::merge_interventions(&mut inputs, history)
            .map_err(bad)?;
    }
    seedbox_engine_protocol::input_program::validate_program(&inputs, request.simulated_time_ns)
        .map_err(bad)?;
    Ok(inputs)
}
fn encode(snapshot: Snapshot, writer: &mut BufWriter<fs::File>) -> Result<()> {
    snapshot.write_to(writer).map_err(bad)
}
struct Assets {
    executable: PathBuf,
    inputs: Option<crate::runtime_assets::Inputs>,
    guest_assets: PathBuf,
    inventory: Inventory,
}
fn materialize_factory(
    inventory: &Inventory,
    output: &Path,
    binding: Option<&wire::RuntimeBinding>,
) -> Result<Assets> {
    super::policy::decode_optional(inventory.backend_policy()).map_err(bad)?;
    use crate::runtime_assets::{Backend, Runtime};
    use seedbox_spec::definition::Architecture;
    let (archive, backend, architecture) = match inventory {
        Inventory::Pv { runtime, .. } => (
            runtime,
            Backend::Pv,
            if cfg!(target_arch = "aarch64") {
                Architecture::Aarch64
            } else {
                Architecture::X86_64
            },
        ),
        Inventory::Tcg { runtime, .. } => (runtime, Backend::Tcg, Architecture::X86_64),
    };
    checkpoint::verified(archive)?;
    let (guest_assets, qemu, inputs) = if let Some(binding) = binding {
        binding.validate_archive(archive).map_err(bad)?;
        checkpoint::verified(&binding.archive)?;
        (binding.guest_assets.clone(), binding.qemu.clone(), None)
    } else {
        // An explicitly unbound standalone worker owns its own extraction. The
        // service supplies a matching binding while holding Inputs in the parent.
        // This is the worker's only executable write (the QEMU launcher and its
        // closure). It must stay here, before `Factory::load` creates any machine,
        // because machine QEMU and PV-worker spawns are not exec-file scoped
        // (see seedbox_artifacts::exec_files): no fork can then copy its fd.
        let runtime = Runtime::from_archive(Path::new(&archive.artifact), output)?;
        verify_at(archive, &runtime.archive())?;
        let inputs = runtime.select(backend, architecture)?;
        (
            inputs.guest_assets.clone(),
            inputs.qemu.clone(),
            Some(inputs),
        )
    };
    let mut inventory = inventory.clone();
    let executable = match &mut inventory {
        Inventory::Pv {
            executable,
            kernel,
            init,
            command_line,
            progress,
            runtime,
            formatter,
            ..
        } => {
            for (artifact, name) in [
                (kernel, "kernel"),
                (init, "init"),
                (command_line, "command-line"),
                (progress, "progress.tar"),
            ] {
                remap(artifact, &guest_assets.join(name))?;
            }
            remap(runtime, Path::new(&archive.artifact))?;
            if let Some(formatter) = formatter {
                remap(formatter, &guest_assets.join("mke2fs"))?;
            }
            let path = output.join("factory-worker");
            copy(executable, &path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o500))?;
            remap(executable, &path)?;
            path
        }
        Inventory::Tcg {
            executable,
            kernel,
            init,
            command_line,
            firmware,
            runtime,
            formatter,
            ..
        } => {
            for (artifact, name) in [
                (kernel, "kernel"),
                (init, "init"),
                (command_line, "command-line"),
                (firmware, "firmware"),
            ] {
                remap(artifact, &guest_assets.join(name))?;
            }
            remap(runtime, Path::new(&archive.artifact))?;
            if let Some(formatter) = formatter {
                remap(formatter, &guest_assets.join("mke2fs"))?;
            }
            // The launcher is used ONLY inside its adjacent retained closure.
            let path = qemu
                .as_ref()
                .ok_or_else(|| bad("TCG runtime has no owned QEMU closure"))?
                .clone();
            remap(executable, &path)?;
            path
        }
    };
    let environments = match &mut inventory {
        Inventory::Pv { environments, .. } | Inventory::Tcg { environments, .. } => environments,
    };
    let directory = output.join("factory-environments");
    fs::create_dir(&directory)?;
    let mut index = 0usize;
    for environment in environments.values_mut() {
        for artifact in environment.configs.values_mut() {
            let path = directory.join(index.to_string());
            index += 1;
            copy(artifact, &path)?;
            remap(artifact, &path)?;
        }
    }
    Ok(Assets {
        executable,
        inputs,
        guest_assets,
        inventory,
    })
}
fn verify_at(artifact: &Artifact, path: &Path) -> Result<()> {
    let mut reference = artifact.clone();
    reference.artifact = path
        .to_str()
        .ok_or_else(|| bad("Driver factory path is not UTF-8"))?
        .into();
    checkpoint::verified(&reference)?;
    Ok(())
}
fn remap(artifact: &mut Artifact, path: &Path) -> Result<()> {
    verify_at(artifact, path)?;
    artifact.artifact = path
        .to_str()
        .ok_or_else(|| bad("Driver factory path is not UTF-8"))?
        .into();
    Ok(())
}
fn copy(artifact: &Artifact, path: &Path) -> Result<()> {
    let mut input = checkpoint::verified(artifact)?;
    let mut file = fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)?;
    let bytes = std::io::copy(
        &mut (&mut input).take(artifact.length.saturating_add(1)),
        &mut file,
    )?;
    if bytes != artifact.length {
        return Err(bad(
            "Driver factory artifact changed during materialization",
        ));
    }
    file.flush()?;
    verify_at(artifact, path)
}
fn definition(input: &wire::Machine) -> Result<MachineDefinition> {
    let mut reader = checkpoint::verified(&input.filesystem)?;
    let bytes = read(&mut reader, input.filesystem.length)?;
    let application = definition::PreparedApplication {
        filesystem: FilesystemArchive::new(bytes).map_err(|e| bad(e.to_string()))?,
        launch: definition::Launch {
            argv: input.process.argv.clone(),
            environment: input.process.environment.clone(),
            working_directory: input.process.working_directory.clone(),
            user: input.process.user.clone(),
        },
    };
    let disk = match &input.disk {
        None => None,
        Some(wire::Disk::Zero { bytes }) => {
            Some(definition::DiskImage::zeroed(*bytes).map_err(|e| bad(e.to_string()))?)
        }
        Some(wire::Disk::Image { contents }) => {
            let mut reader = checkpoint::verified(contents)?;
            Some(
                definition::DiskImage::from_bytes(read(&mut reader, contents.length)?)
                    .map_err(|e| bad(e.to_string()))?,
            )
        }
    };
    let network = input
        .interface
        .as_ref()
        .map(|interface| {
            definition::NetworkInterface::new(
                definition::MacAddress::new(interface.mac)?,
                interface.address,
                interface.prefix_len,
            )
        })
        .transpose()
        .map_err(|e| bad(e.to_string()))?;
    Ok(MachineDefinition {
        name: input.name.clone(),
        resources: definition::Resources {
            memory_bytes: input.memory_bytes,
        },
        application,
        disk,
        network,
    })
}
fn read(reader: &mut impl Read, length: u64) -> Result<Vec<u8>> {
    let length =
        usize::try_from(length).map_err(|_| bad("Driver artifact exceeds host address space"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| bad("Driver artifact allocation failed"))?;
    bytes.resize(length, 0);
    reader.read_exact(&mut bytes)?;
    let mut extra = [0];
    if reader.read(&mut extra)? != 0 {
        return Err(bad("Driver artifact exceeds retained length"));
    }
    Ok(bytes)
}
fn allocation(encoded: u64) -> Result<u64> {
    encoded
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(64 << 20))
        .ok_or_else(|| bad("Driver decode allocation budget overflow"))
}
fn driver_limits(encoded: u64) -> Result<seedbox_driver::driver::DecodeLimits> {
    Ok(seedbox_driver::driver::DecodeLimits {
        encoded_bytes: encoded,
        allocation_bytes: allocation(encoded)?,
        // The writer has no separate string/name/collection semantic cap.
        // Bound decoded resources by retained encoded bytes, not new pins that
        // make a successfully captured long handle or large map unrestorable.
        collection_items: encoded,
        total_items: encoded.saturating_mul(4),
        string_bytes: encoded,
        blob_bytes: encoded,
        depth: 128,
        archive_entries: encoded,
        archive_path_bytes: encoded,
        archive_expanded_bytes: encoded,
    })
}
fn native_limits(encoded: u64) -> Result<seedbox_machine::snapshot::DecodeLimits> {
    Ok(seedbox_machine::snapshot::DecodeLimits {
        encoded_bytes: encoded,
        allocation_bytes: allocation(encoded)?,
        collection_items: encoded,
        total_items: encoded.saturating_mul(4),
        string_bytes: encoded,
        blob_bytes: encoded,
    })
}
