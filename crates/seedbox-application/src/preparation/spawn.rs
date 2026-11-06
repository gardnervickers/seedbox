//! Prepare one machine an oracle spawn asks for, while its run waits. This is
//! the same image import and filesystem view the project's machines get; the
//! worker builds the boot archive from them.
use super::*;
use seedbox_driver_execution::driver_world::preparation::{artifact, MachineInputs};
use seedbox_engine_protocol::driver_world as wire;

impl OciPreparer {
    /// Prepare the machine `request` names from its spec, a `machines` entry
    /// of `simulation`'s seedbox.yaml. Files the worker reads are written
    /// under `directory`; `runtime` is the run's selected Driver runtime.
    pub fn prepare_spawn(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        transport: &dyn crate::oci::ImageTransport,
        runtime: &seedbox_driver_execution::runtime_assets::Inputs,
        request: &wire::SpawnRequest,
        directory: &Path,
    ) -> Result<(wire::Machine, wire::Environment)> {
        let architecture = crate::guest::Architecture::from_profile(&self.profile)?;
        let name = &request.name;
        let (service, files) =
            crate::config::spawned_machine(&request.spec, name, simulation, architecture)?;
        let memory_mb = service.memory_mb.unwrap_or(simulation.machine.memory_mb);
        validate_machine_memory(&self.profile, name, memory_mb)?;
        for mount in &service.volumes {
            validate_config_target(&mount.target)?;
        }
        if !service.volumes.is_empty() && !runtime.guest_assets.join("mke2fs").is_file() {
            return Err(missing_formatter());
        }
        fs::create_dir_all(directory)?;
        let mut configs = BTreeMap::new();
        for (index, mount) in service.configs.iter().enumerate() {
            validate_config_target(&mount.target)?;
            let source = files
                .get(&mount.source)
                .ok_or_else(|| Error::Integrity("unknown config source".into()))?;
            let bytes = read_bounded(
                fs::File::open(source)?,
                CONFIG_LIMIT,
                format_args!("config {}", mount.source),
            )
            .map_err(|e| e.or_too_large(Error::Execution))?;
            let path = directory.join(format!("config-{index}"));
            fs::write(&path, bytes)?;
            configs.insert(mount.target.clone(), artifact(&path)?);
        }
        let temporary = tempfile::tempdir()?;
        let imported = transport
            .import(&service, temporary.path(), store, None, architecture)?
            .for_service(&service, store)?;
        let filesystem = directory.join("filesystem");
        fs::write(
            &filesystem,
            crate::driver_preparation::filesystem(&imported.rootfs)?,
        )?;
        MachineInputs {
            name,
            number: request.number,
            memory_bytes: u64::from(memory_mb) * 1024 * 1024,
            filesystem: artifact(&filesystem)?,
            launch: &imported.image.launch,
            configs,
            allow_crashes: service.allow_crashes,
            control_socket: false,
        }
        .wire()
    }
}
