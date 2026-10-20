//! The resolved shape of a native project. The CLI checks its own resolution
//! and the server checks submitted project inputs against this one contract.
use super::*;

/// Check a resolved native project: only what `seedbox.yaml` can express,
/// with host paths already resolved. Settings the format cannot express must
/// keep their defaults; a resolved definition carries anything else.
pub fn validate_project(simulation: &Simulation) -> Result<()> {
    let services = &simulation.services;
    if !services.contains_key("oracle") {
        return invalid("$.services.oracle", "the oracle machine is required");
    }
    if services.len() > 254 {
        return invalid(
            "$.services",
            "at most 253 application machines are supported",
        );
    }
    for (name, service) in services {
        validate_service(simulation, name, service)?;
    }
    if simulation.configs.values().any(|path| !path.is_absolute()) {
        return invalid("$.configs", "initial files must be absolute host paths");
    }
    let network = &simulation.network;
    if network.name != "default"
        || network.jitter_ns != 0
        || network.bandwidth_bps.is_some()
        || network.queue_bound_ns.is_some()
        || !network.links.is_empty()
    {
        return invalid("$.network", "project networks set only their latency");
    }
    let oracle = || "oracle".to_owned();
    for (unexpressible, setting) in [
        (simulation.schedule != Schedule::default(), "schedule"),
        (simulation.machine != MachineOptions::default(), "machine"),
        (simulation.disk != DiskOptions::default(), "disk"),
        (simulation.wall_clock_epoch.is_some(), "wall_clock_epoch"),
        (
            simulation.limits.simulated_time_ns != u64::MAX,
            "limits.simulated_time_ns",
        ),
        (
            simulation.check
                != (Check {
                    service: oracle(),
                    exit_code: 0,
                }),
            "check",
        ),
        (
            simulation.control != Some(Control { service: oracle() }),
            "control",
        ),
    ] {
        if unexpressible {
            return invalid(
                &format!("$.{setting}"),
                "projects keep the default; submit a resolved definition to change it",
            );
        }
    }
    Ok(())
}

fn validate_service(simulation: &Simulation, name: &str, service: &Service) -> Result<()> {
    let path = format!("$.services.{name}");
    if !dns_label(name) {
        return invalid(
            &path,
            "machine names must be lowercase DNS labels (1 to 63 characters)",
        );
    }
    // An empty platform is bound to the execution server's architecture, and
    // its memory limit, when the server accepts the submission.
    let maximum = match service.platform.as_str() {
        "" => 65536,
        platform if platform == crate::guest::Architecture::Amd64.platform() => 65536,
        platform if platform == crate::guest::Architecture::Arm64.platform() => 3072,
        _ => return invalid(&format!("{path}.platform"), "unsupported guest platform"),
    };
    if service
        .memory_mb
        .is_some_and(|memory| !(64..=maximum).contains(&memory))
    {
        return invalid(
            &format!("{path}.memory_mb"),
            format!("expected 64..{maximum} MiB"),
        );
    }
    match &service.source {
        ImageSource::Registry { reference } if !reference.is_empty() => {}
        ImageSource::Build {
            context,
            dockerfile,
        } if context.is_absolute() && dockerfile.is_absolute() => {}
        _ => {
            return invalid(
                &format!("{path}.source"),
                "expected an image reference or absolute host build paths",
            )
        }
    }
    if let Some(directory) = &service.working_dir {
        guest_path(directory, &format!("{path}.working_dir"))?;
    }
    if service
        .entrypoint
        .iter()
        .flatten()
        .chain(service.command.iter().flatten())
        .chain(service.user.iter())
        .any(|value| value.contains('\0'))
    {
        return invalid(&path, "launch fields must not contain NUL");
    }
    if service.environment.iter().any(|(key, value)| {
        key.is_empty()
            || key.contains(['=', '\0'])
            || value.as_ref().is_some_and(|v| v.contains('\0'))
    }) {
        return invalid(
            &format!("{path}.environment"),
            "environment names must be nonempty and contain no '='; names and values must contain no NUL",
        );
    }
    let mut targets: Vec<&str> = vec![];
    for mount in &service.configs {
        guest_path(&mount.target, &format!("{path}.configs"))?;
        if mount.target == "/" || !simulation.configs.contains_key(&mount.source) {
            return invalid(
                &format!("{path}.configs"),
                "initial files need a declared source and cannot replace the guest root",
            );
        }
        if targets.iter().any(|target| overlaps(target, &mount.target)) {
            return invalid(&format!("{path}.configs"), "initial file targets overlap");
        }
        targets.push(&mount.target);
    }
    for volume in &service.volumes {
        guest_path(&volume.target, &format!("{path}.volumes"))?;
        if !identifier(&volume.source)
            || volume.target == "/"
            || targets
                .iter()
                .any(|target| overlaps(target, &volume.target))
        {
            return invalid(
                &format!("{path}.volumes"),
                "volume targets must not overlap files, other volumes, or the guest root",
            );
        }
        targets.push(&volume.target);
    }
    if service.network != "default" {
        return invalid(
            &format!("{path}.network"),
            "project machines join the default network",
        );
    }
    Ok(())
}

pub(super) fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved() -> (tempfile::TempDir, Simulation) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("inputs")).unwrap();
        std::fs::write(temp.path().join("inputs/settings"), "hello").unwrap();
        let text = "name: demo\nmachines:\n  app:\n    image: app\n    files: [{source: inputs, target: /etc/demo}]\n    volumes: {data: {target: /data}}\noracle:\n  image: test\n";
        let simulation = parse_for_architecture(
            text,
            &temp.path().join("seedbox.yaml"),
            &BTreeMap::new(),
            crate::guest::Architecture::Amd64,
        )
        .unwrap();
        (temp, simulation)
    }

    #[test]
    fn resolved_projects_pass_including_a_server_bound_platform() {
        let (_temp, mut simulation) = resolved();
        validate_project(&simulation).unwrap();
        for service in simulation.services.values_mut() {
            service.platform.clear();
        }
        validate_project(&simulation).unwrap();
    }

    #[test]
    fn rejects_what_a_project_cannot_express() {
        let (_temp, simulation) = resolved();
        let app = |edit: fn(&mut Service)| {
            let mut simulation = simulation.clone();
            edit(simulation.services.get_mut("app").unwrap());
            simulation
        };
        let cases: [(&str, Simulation); 12] = [
            ("$.services.oracle", {
                let mut s = simulation.clone();
                s.services.remove("oracle");
                s
            }),
            ("$.services.App", {
                let mut s = simulation.clone();
                let app = s.services.remove("app").unwrap();
                s.services.insert("App".into(), app);
                s
            }),
            (
                "$.services.app.source",
                app(|s| {
                    s.source = ImageSource::Build {
                        context: "relative".into(),
                        dockerfile: "/Dockerfile".into(),
                    }
                }),
            ),
            (
                "$.services.app.memory_mb",
                app(|s| s.memory_mb = Some(65537)),
            ),
            (
                "$.services.app",
                app(|s| s.command = Some(vec!["a\0b".into()])),
            ),
            (
                "$.services.app.volumes",
                app(|s| s.volumes[0].target = "/etc".into()),
            ),
            (
                "$.services.app.configs",
                app(|s| s.configs[0].source = "undeclared".into()),
            ),
            ("$.network", {
                let mut s = simulation.clone();
                s.network.jitter_ns = 1;
                s
            }),
            ("$.schedule", {
                let mut s = simulation.clone();
                s.schedule.stop_tolerance_ns = 1;
                s
            }),
            ("$.check", {
                let mut s = simulation.clone();
                s.check.exit_code = 1;
                s
            }),
            ("$.wall_clock_epoch", {
                let mut s = simulation.clone();
                s.wall_clock_epoch = Some(1);
                s
            }),
            ("$.configs", {
                let mut s = simulation.clone();
                let key = s.services["app"].configs[0].source.clone();
                s.configs.insert(key, "relative".into());
                s
            }),
        ];
        for (path, simulation) in cases {
            match validate_project(&simulation) {
                Err(Error::Validation { path: actual, .. }) => assert_eq!(actual, path),
                other => panic!("{path}: expected a validation error, got {other:?}"),
            }
        }
    }
}
