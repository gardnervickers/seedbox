//! Resolve native project inputs without contacting an image engine.
use super::{project_form::overlaps, *};
use seedbox_spec::project::{Machine, Project};

pub(super) fn parse(
    doc: Value,
    source: &Path,
    architecture: crate::guest::Architecture,
) -> Result<Simulation> {
    let project: Project = serde_yaml_ng::from_value(doc)?;
    if !dns_label(&project.name) {
        return invalid(
            "$.name",
            "expected a lowercase DNS label (1 to 63 characters)",
        );
    }
    if project.machines.len() > 253 {
        return invalid(
            "$.machines",
            "at most 253 application machines are supported",
        );
    }
    if project.machines.contains_key("oracle") {
        return invalid(
            "$.machines.oracle",
            "oracle is reserved; declare it in the top-level oracle section",
        );
    }
    let base = source.parent().unwrap_or(Path::new("."));
    let mut configs = BTreeMap::new();
    let mut services = BTreeMap::new();
    for (name, machine) in project
        .machines
        .into_iter()
        .chain(std::iter::once(("oracle".into(), project.oracle)))
    {
        let path = if name == "oracle" {
            "$.oracle".into()
        } else {
            format!("$.machines.{name}")
        };
        if !dns_label(&name) {
            return invalid(
                &path,
                "machine names must be lowercase DNS labels (1 to 63 characters)",
            );
        }
        services.insert(
            name.clone(),
            resolve_machine(machine, &name, &path, base, architecture, &mut configs)?,
        );
    }
    let latency_ns = duration(
        &Value::String(project.network.latency),
        "$.network.latency",
        false,
    )?;
    let quantum_ns = duration(
        &Value::String(project.simulator.quantum),
        "$.simulator.quantum",
        false,
    )?;
    let simulation = Simulation {
        name: project.name,
        source: source.to_owned(),
        services,
        configs,
        network: Network {
            name: "default".into(),
            latency_ns,
            jitter_ns: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
            links: vec![],
        },
        schedule: Schedule::default(),
        control: Some(Control {
            service: "oracle".into(),
        }),
        // Private engine adapter fields until execution options are separated.
        // No simulated-time cutoff is exposed by the native authoring format.
        limits: Limits {
            simulated_time_ns: u64::MAX,
            wall_time_ns: 600_000_000_000,
        },
        check: Check {
            service: "oracle".into(),
            exit_code: 0,
        },
        seed: None,
        wall_clock_epoch: None,
        machine: MachineOptions::default(),
        disk: DiskOptions::default(),
        simulator: seedbox_spec::SimulatorOptions {
            progress_budget: project.simulator.progress_budget,
            quantum_ns: std::num::NonZeroU64::new(quantum_ns)
                .ok_or_else(|| validation("$.simulator.quantum", "expected a positive duration"))?,
            machine_workers: project.simulator.machine_workers,
        },
    };
    validate_project(&simulation)?;
    Ok(simulation)
}

fn resolve_machine(
    machine: Machine,
    name: &str,
    path: &str,
    base: &Path,
    architecture: crate::guest::Architecture,
    configs: &mut BTreeMap<String, PathBuf>,
) -> Result<Service> {
    if let Some(memory) = machine.memory_mib {
        let maximum = match architecture {
            crate::guest::Architecture::Arm64 => 3072,
            crate::guest::Architecture::Amd64 => 65536,
        };
        if !(64..=maximum).contains(&memory.get()) {
            return invalid(
                &format!("{path}.memoryMiB"),
                format!("expected 64..{maximum} MiB"),
            );
        }
    }
    let source = match (machine.image, machine.build) {
        (Some(reference), None) => ImageSource::Registry { reference },
        (None, Some(build)) => {
            let context = local_path(
                &Value::String(build.context.to_string_lossy().into_owned()),
                &format!("{path}.build.context"),
                base,
                true,
            )?;
            let dockerfile = local_path(
                &Value::String(build.dockerfile.to_string_lossy().into_owned()),
                &format!("{path}.build.dockerfile"),
                &context,
                false,
            )?;
            ImageSource::Build {
                context,
                dockerfile,
            }
        }
        _ => return invalid(path, "specify exactly one of image or build"),
    };
    if let Some(directory) = &machine.working_directory {
        guest_path(directory, &format!("{path}.workingDirectory"))?;
    }
    for (key, value) in &machine.environment {
        if key.is_empty()
            || key.contains(['=', '\0'])
            || value.as_ref().is_some_and(|v| v.contains('\0'))
        {
            return invalid(
                &format!("{path}.environment"),
                "environment names must be nonempty and contain no '='; names and values must contain no NUL",
            );
        }
    }
    for args in [&machine.entrypoint, &machine.command]
        .into_iter()
        .flatten()
    {
        if args.iter().any(|arg| arg.contains('\0')) {
            return invalid(path, "process arguments must not contain NUL");
        }
    }
    let mut mounts = vec![];
    for file in machine.files {
        guest_path(&file.target, &format!("{path}.files.target"))?;
        if file.target == "/" {
            return invalid(path, "initial files cannot replace the guest root");
        }
        collect_files(
            &base.join(file.source),
            &file.target,
            name,
            path,
            configs,
            &mut mounts,
        )?;
    }
    let mut volumes: Vec<VolumeMount> = vec![];
    for (volume, options) in machine.volumes {
        if !identifier(&volume) {
            return invalid(path, "invalid volume name");
        }
        guest_path(&options.target, &format!("{path}.volumes.{volume}.target"))?;
        if options.target == "/"
            || mounts.iter().any(|m| overlaps(&m.target, &options.target))
            || volumes.iter().any(|m| overlaps(&m.target, &options.target))
        {
            return invalid(
                path,
                "volume targets must not overlap files, other volumes, or the guest root",
            );
        }
        volumes.push(VolumeMount {
            source: format!("{name}-{volume}"),
            target: options.target,
        });
    }
    Ok(Service {
        memory_mb: machine.memory_mib.map(|memory| memory.get()),
        source,
        platform: architecture.platform().into(),
        entrypoint: machine.entrypoint,
        command: machine.command,
        environment: machine.environment,
        working_dir: machine.working_directory,
        user: machine.user,
        configs: mounts,
        volumes,
        network: "default".into(),
    })
}

fn collect_files(
    source: &Path,
    target: &str,
    machine: &str,
    path: &str,
    configs: &mut BTreeMap<String, PathBuf>,
    mounts: &mut Vec<ConfigMount>,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|e| validation(path, &format!("{}: {e}", source.display())))?;
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(source)
            .map_err(|e| validation(path, &e.to_string()))?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| validation(path, &e.to_string()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| validation(path, "initial file names must be UTF-8"))?;
            collect_files(
                &entry.path(),
                &format!("{target}/{name}"),
                machine,
                path,
                configs,
                mounts,
            )?;
        }
    } else if metadata.is_file() {
        if mounts.iter().any(|m| overlaps(&m.target, target)) {
            return invalid(path, "initial file targets overlap");
        }
        if configs.len() >= 65536 {
            return invalid(path, "too many initial files");
        }
        let key = format!("{machine}-file-{}", mounts.len());
        let canonical = source
            .canonicalize()
            .map_err(|e| validation(path, &e.to_string()))?;
        configs.insert(key.clone(), canonical);
        mounts.push(ConfigMount {
            source: key,
            target: target.into(),
        });
    } else {
        return invalid(
            path,
            format!(
                "{}: initial inputs must be regular files or directories (no symlinks)",
                source.display()
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_native(text: &str) -> Result<Simulation> {
        super::super::parse_for_architecture(
            text,
            Path::new("/tmp/seedbox.yaml"),
            &BTreeMap::new(),
            crate::guest::Architecture::Arm64,
        )
    }

    #[test]
    fn oracle_and_defaults_resolve_without_host_environment() {
        let sim = parse_native("name: demo\nmachines:\n  app: {image: app}\noracle:\n  image: test\n  environment: {HOME: null}\nnetwork: {latency: 2.5ms}\n").unwrap();
        assert_eq!(sim.check.service, "oracle");
        assert_eq!(sim.control.unwrap().service, "oracle");
        assert_eq!(sim.services["oracle"].environment["HOME"], None);
        assert_eq!(sim.services["app"].platform, "linux/arm64");
        assert_eq!(sim.network.latency_ns, 2_500_000);
        assert_eq!(sim.limits.simulated_time_ns, u64::MAX);
        assert_eq!(sim.simulator.quantum_ns.get(), 1_000_000);
        assert_eq!(sim.simulator.machine_workers.get(), 1);
    }

    #[test]
    fn simulator_resolves_quantum_and_machine_capacity() {
        let sim = parse_native(
            "name: demo\noracle: {image: test}\nsimulator: {quantum: 2.5ms, machineWorkers: 4}\n",
        )
        .unwrap();
        assert_eq!(sim.simulator.quantum_ns.get(), 2_500_000);
        assert_eq!(sim.simulator.machine_workers.get(), 4);
    }

    #[test]
    fn simulator_rejects_invalid_quantum_and_capacity() {
        for quantum in ["0ns", "18446744073709551616ns", "0.1ns"] {
            let err = parse_native(&format!(
                "name: demo\noracle: {{image: test}}\nsimulator: {{quantum: {quantum}}}\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains("$.simulator.quantum"), "{err}");
        }
        for workers in [0, 65] {
            assert!(parse_native(&format!(
                "name: demo\noracle: {{image: test}}\nsimulator: {{machineWorkers: {workers}}}\n"
            ))
            .is_err());
        }
    }

    #[test]
    fn rejects_reserved_names_and_invalid_latency() {
        for yaml in [
            "name: demo\nmachines: {oracle: {image: app}}\noracle: {image: test}",
            "name: demo\noracle: {image: test}\nnetwork: {latency: nonsense}",
        ] {
            assert!(parse_native(yaml).is_err());
        }
    }
    #[test]
    fn captures_directory_files_and_scopes_volumes_to_the_machine() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("inputs")).unwrap();
        std::fs::write(temp.path().join("inputs/settings"), "hello").unwrap();
        let text = "name: demo\nmachines:\n  app:\n    image: app\n    files: [{source: inputs, target: /etc/demo}]\n    volumes: {data: {target: /data}}\noracle:\n  image: test\n  volumes: {data: {target: /data}}\n";
        let sim = super::super::parse_for_architecture(
            text,
            &temp.path().join("seedbox.yaml"),
            &BTreeMap::new(),
            crate::guest::Architecture::Arm64,
        )
        .unwrap();
        assert_eq!(sim.services["app"].configs[0].target, "/etc/demo/settings");
        assert_ne!(
            sim.services["app"].volumes[0].source,
            sim.services["oracle"].volumes[0].source
        );
        let overlap = text.replace("target: /data", "target: /etc");
        assert!(super::super::parse_for_architecture(
            &overlap,
            &temp.path().join("seedbox.yaml"),
            &BTreeMap::new(),
            crate::guest::Architecture::Arm64
        )
        .is_err());
    }
}
