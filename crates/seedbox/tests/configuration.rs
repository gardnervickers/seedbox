use seedbox::{
    config::{self, ImageSource},
    image::{resolve_launch, ImageDefaults},
};
use std::{collections::BTreeMap, path::Path};

const BASE: &str = "name: test\nservices:\n  workload:\n    image: example/workload:v1\nx-seedbox:\n  version: 1\n  seed: 1009\n  limits: {simulated_time: 120s, wall_time: 30m}\n  network: {latency: 1ms, jitter: 200us}\n  check: {service: workload, exit_code: 0}\n";

fn parse(yaml: &str) -> seedbox::Result<config::Simulation> {
    config::parse(yaml, Path::new("/fixture/compose.yaml"), &BTreeMap::new())
}

#[test]
fn validates_and_normalizes_the_authoring_subset() {
    let sim = parse(BASE).unwrap();
    assert_eq!(sim.seed, Some(1009));
    assert_eq!(sim.limits.simulated_time_ns, 120_000_000_000);
    assert_eq!(sim.limits.wall_time_ns, 1_800_000_000_000);
    assert_eq!(sim.network.jitter_ns, 200_000);
    assert_eq!(sim.services["workload"].platform, "linux/amd64");
    assert_eq!(sim.services["workload"].entrypoint, None);
}

#[test]
fn schedule_controls_are_explicit_semantic_durations() {
    assert_eq!(parse(BASE).unwrap().schedule, config::Schedule::default());
    let yaml = BASE.replace(
        "  version: 1",
        "  version: 1\n  schedule: {timer_jitter: 1ms, stop_tolerance: 200us}",
    );
    let schedule = parse(&yaml).unwrap().schedule;
    assert_eq!(schedule.timer_jitter_ns, 1_000_000);
    assert_eq!(schedule.stop_tolerance_ns, 200_000);
    assert!(parse(&yaml.replace("timer_jitter: 1ms", "timer_jitter: -1ms")).is_err());
    assert!(parse(&yaml.replace("stop_tolerance:", "unknown:"))
        .unwrap_err()
        .to_string()
        .contains("$.x-seedbox.schedule.unknown"));
}

#[test]
fn image_metadata_accepts_absent_or_null_optional_collections() {
    let defaults: ImageDefaults =
        serde_json::from_str(r#"{"Cmd":["app"],"Env":null,"Volumes":null}"#).unwrap();
    let sim = parse(BASE).unwrap();
    let launch = resolve_launch(&sim.services["workload"], &defaults).unwrap();
    assert!(launch.environment.is_empty());
    assert!(launch.volumes.is_empty());
    assert_eq!(launch.working_dir, "/");
    assert_eq!(launch.argv, ["app"]);
}

#[test]
fn unknown_and_unsupported_fields_are_errors_at_their_paths() {
    let cases = [
        (
            BASE.replace("    image:", "    volumes: [./data:/data]\n    image:"),
            "$.services.workload.volumes",
        ),
        (
            BASE.replace("    image:", "    ports: ['80:80']\n    image:"),
            "$.services.workload.ports",
        ),
        (
            BASE.replace("    image:", "    network_mode: host\n    image:"),
            "$.services.workload.network_mode",
        ),
        (
            BASE.replace("    image:", "    privileged: true\n    image:"),
            "$.services.workload.privileged",
        ),
        (
            BASE.replace("    image:", "    depends_on: [db]\n    image:"),
            "$.services.workload.depends_on",
        ),
        (
            BASE.replace("    image:", "    platform: linux/arm64\n    image:"),
            "$.services.workload.platform",
        ),
        (
            BASE.replace("    image:", "    command: 'echo $HOME'\n    image:"),
            "$.services.workload.command",
        ),
        (
            BASE.replace("  version: 1", "  version: 1\n  surprise: true"),
            "$.x-seedbox.surprise",
        ),
        (
            BASE.replace(
                "  version: 1",
                "  version: 1\n  faults: [{at: 10s, restart: {service: workload, downtime: 1s}}]",
            ),
            "$.x-seedbox.faults",
        ),
        (
            BASE.replace("service: workload", "service: absent"),
            "$.x-seedbox.check.service",
        ),
        (
            BASE.replace("exit_code: 0", "exit_code: 256"),
            "$.x-seedbox.check.exit_code",
        ),
        (BASE.replace("seed: 1009", "seed: -1"), "$.x-seedbox.seed"),
        (BASE.replace("seed: 1009", "seed: true"), "$.x-seedbox.seed"),
        (
            BASE.replace("latency: 1ms", "latency: 200us"),
            "$.x-seedbox.network.jitter",
        ),
        (
            BASE.replace("    image:", "    working_dir: ../host\n    image:"),
            "$.services.workload.working_dir",
        ),
        (
            BASE.replace("    image:", "    build: .\n    image:"),
            "$.services.workload",
        ),
        (format!("{BASE}include: other.yaml\n"), "$.include"),
    ];
    for (yaml, path) in cases {
        let error = parse(&yaml).unwrap_err().to_string();
        assert!(error.contains(path), "expected {path}, got {error}");
    }
}

#[test]
fn durations_are_exact_and_checked_without_floating_point() {
    for (raw, expected) in [
        ("0.000000001s", 1),
        ("1.25ms", 1_250_000),
        ("18446744073709551615ns", u64::MAX),
    ] {
        let sim = parse(&BASE.replace("120s", raw)).unwrap();
        assert_eq!(sim.limits.simulated_time_ns, expected);
    }
    for raw in [
        "0s",
        "-1s",
        "1e3s",
        "0.1ns",
        "NaNs",
        "18446744073709551616ns",
        "999999999999999999999999999999999999999999999h",
        "1.s",
        "1.2.3s",
    ] {
        assert!(parse(&BASE.replace("120s", raw)).is_err(), "accepted {raw}");
    }
}

#[test]
fn rejects_duplicate_keys_tags_merges_and_ambiguous_environment_scalars() {
    for yaml in [
        BASE.replace("  version: 1", "  version: 1\n  version: 2"),
        BASE.replace("    image:", "    environment: {X: a, X: b}\n    image:"),
        BASE.replace("    image:", "    <<: {command: [bad]}\n    image:"),
        BASE.replace("    image:", "    command: !custom [hello]\n    image:"),
        BASE.replace("    image:", "    environment: {X: true}\n    image:"),
        BASE.replace("    image:", "    environment: {X: 12}\n    image:"),
        BASE.replace("    image:", "    environment: [X, X]\n    image:"),
    ] {
        assert!(parse(&yaml).is_err(), "accepted {yaml}");
    }
}

#[test]
fn environment_uses_only_explicit_input_and_preserves_empty_and_unset() {
    let yaml = BASE.replace(
        "    image:",
        "    environment: [FROM_HOST, MISSING, EMPTY=, LITERAL=a=b]\n    image:",
    );
    let supplied = BTreeMap::from([
        ("FROM_HOST".into(), "provided $literally".into()),
        ("UNUSED".into(), "not inherited".into()),
    ]);
    let sim = config::parse(&yaml, Path::new("/fixture/compose.yaml"), &supplied).unwrap();
    assert_eq!(
        sim.services["workload"].environment,
        BTreeMap::from([
            ("FROM_HOST".into(), Some("provided $literally".into())),
            ("MISSING".into(), None),
            ("EMPTY".into(), Some("".into())),
            ("LITERAL".into(), Some("a=b".into())),
        ])
    );
    assert_eq!(
        parse(&yaml).unwrap().services["workload"].environment["FROM_HOST"],
        None
    );
}

#[test]
fn command_strings_split_without_implicitly_invoking_a_shell() {
    let yaml = BASE.replace(
        "    image:",
        "    command: 'echo \"two words\" $$HOME'\n    image:",
    );
    let sim = parse(&yaml).unwrap();
    assert_eq!(
        sim.services["workload"].command.as_ref().unwrap(),
        &["echo", "two words", "$HOME"]
    );
    assert!(
        parse(&BASE.replace("    image:", "    command: \"'unterminated\"\n    image:")).is_err()
    );
}

#[test]
fn image_entrypoint_and_cmd_follow_compose_override_rules() {
    let image = ImageDefaults {
        entrypoint: Some(vec!["entry".into()]),
        cmd: Some(vec!["default".into()]),
        ..Default::default()
    };
    // None means inherit; Some(empty) clears. Explicit entrypoint drops image CMD.
    let cases = [
        ("", vec!["entry", "default"]),
        (
            "    entrypoint: null\n    command: null\n",
            vec!["entry", "default"],
        ),
        ("    command: [custom]\n", vec!["entry", "custom"]),
        ("    command: []\n", vec!["entry"]),
        ("    command: ''\n", vec!["entry"]),
        ("    entrypoint: [override]\n", vec!["override"]),
        (
            "    entrypoint: [override]\n    command: null\n",
            vec!["override"],
        ),
        (
            "    entrypoint: [override]\n    command: [custom]\n",
            vec!["override", "custom"],
        ),
        (
            "    entrypoint: []\n    command: [executable]\n",
            vec!["executable"],
        ),
        (
            "    entrypoint: ''\n    command: [executable]\n",
            vec!["executable"],
        ),
    ];
    for (overrides, expected) in cases {
        let sim = parse(&BASE.replace("    image:", &format!("{overrides}    image:"))).unwrap();
        assert_eq!(
            resolve_launch(&sim.services["workload"], &image)
                .unwrap()
                .argv,
            expected,
            "{overrides}"
        );
    }
    for overrides in [
        "    entrypoint: []\n",
        "    entrypoint: ''\n",
        "    entrypoint: []\n    command: []\n",
    ] {
        let sim = parse(&BASE.replace("    image:", &format!("{overrides}    image:"))).unwrap();
        assert!(resolve_launch(&sim.services["workload"], &image).is_err());
    }
}

#[test]
fn launch_merges_environment_and_preserves_image_user_workdir_and_volumes() {
    let sim = parse(&BASE.replace(
        "    image:",
        "    environment: {REMOVE: null, EMPTY: '', OVERRIDE: service}\n    image:",
    ))
    .unwrap();
    let image = ImageDefaults {
        cmd: Some(vec!["app".into()]),
        env: vec![
            "REMOVE=yes".into(),
            "EMPTY=image".into(),
            "OVERRIDE=image".into(),
            "KEEP=a=b".into(),
        ],
        working_dir: "/app".into(),
        user: "1000:1000".into(),
        volumes: BTreeMap::from([("/data".into(), serde_json::json!({}))]),
        ..Default::default()
    };
    let launch = resolve_launch(&sim.services["workload"], &image).unwrap();
    assert_eq!(
        launch.environment,
        BTreeMap::from([
            ("EMPTY".into(), "".into()),
            ("OVERRIDE".into(), "service".into()),
            ("KEEP".into(), "a=b".into())
        ])
    );
    assert_eq!(launch.working_dir, "/app");
    assert_eq!(launch.user, "1000:1000");
    assert_eq!(launch.volumes, ["/data"]);
}

#[test]
fn build_and_config_paths_are_relative_to_compose_not_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let context = directory.path().join("app");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(context.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(directory.path().join("settings.conf"), "hello").unwrap();
    let yaml = BASE.replace(
        "    image: example/workload:v1",
        "    build: ./app\n    configs: [settings, {source: settings, target: /etc/second.conf}]",
    ) + "configs:\n  settings: {file: ./settings.conf}\n";
    let path = directory.path().join("compose.yaml");
    std::fs::write(&path, yaml).unwrap();
    let sim = config::load(&path, &BTreeMap::new()).unwrap();
    assert_eq!(
        sim.services["workload"].source,
        ImageSource::Build {
            context: context.canonicalize().unwrap(),
            dockerfile: context.join("Dockerfile").canonicalize().unwrap()
        }
    );
    assert_eq!(sim.services["workload"].configs[0].target, "/settings");
    std::fs::remove_file(context.join("Dockerfile")).unwrap();
    assert!(config::load(&path, &BTreeMap::new())
        .unwrap_err()
        .to_string()
        .contains("$.services.workload.build.dockerfile"));
}

#[test]
fn network_options_and_unimplemented_config_permissions_are_not_ignored() {
    let custom = BASE.replace("    image:", "    networks: [private]\n    image:")
        + "networks: {private: {}}\n";
    assert_eq!(parse(&custom).unwrap().network.name, "private");
    assert!(parse(&(BASE.to_owned() + "networks: {private: {}}\n")).is_err());
    assert!(
        parse(&custom.replace("private: {}", "private: {external: true}"))
            .unwrap_err()
            .to_string()
            .contains("$.networks.private.external")
    );
    assert!(parse(&custom.replace(
        "networks: [private]",
        "networks: {private: {aliases: [other]}}"
    ))
    .unwrap_err()
    .to_string()
    .contains("$.services.workload.networks.private.aliases"));
}

#[test]
fn runtime_control_selects_known_service_and_static_schedules_are_rejected() {
    let yaml = BASE.replace(
        "  version: 1",
        "  version: 1\n  control: {service: workload}",
    );
    assert_eq!(parse(&yaml).unwrap().control.unwrap().service, "workload");
    assert!(parse(&yaml.replace(
        "control: {service: workload}",
        "control: {service: missing}"
    ))
    .is_err());
    assert!(parse(&BASE.replace(
        "  version: 1",
        "  version: 1\n  faults: [{at: 1s, stop: workload}] "
    ))
    .is_err());
    assert!(parse(&BASE.replace("  version: 1", "  version: 1\n  faults: []")).is_ok());
    assert!(serde_json::to_value(parse(BASE).unwrap())
        .unwrap()
        .get("control")
        .is_none());
}

#[test]
fn directed_baselines_validate_services_pairs_floor_jitter_and_loss_at_json_paths() {
    let base = BASE.replace(
        "services:\n",
        "services:\n  server:\n    image: example/server:v1\n",
    );
    let parse_link = |entries: &str| {
        parse(&base.replace(
            "network: {latency: 1ms, jitter: 200us}",
            &format!("network: {{latency: 1ms, links: [{entries}]}}"),
        ))
    };
    let good = "{from: server, to: workload, latency: 1ms}";
    let sim = parse_link(good).unwrap();
    assert_eq!(
        sim.network.links[0],
        config::Link {
            from: "server".into(),
            to: "workload".into(),
            latency_ns: 1_000_000,
            jitter_ns: 0,
            loss_ppm: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
        }
    );
    assert!(parse_link("{from: server, to: workload, latency: 2ms, jitter: 1ms, loss_ppm: 1000000}, {from: workload, to: server, latency: 1ms}").is_ok());
    for (entry, path) in [
        (format!("{good}, {good}"), "links[1]"),
        (
            good.replace("from: server", "from: missing"),
            "links[0].from",
        ),
        (good.replace("to: workload", "to: missing"), "links[0].to"),
        (good.replace("1ms", "999us"), "links[0].latency"),
        (good.replace("1ms}", "1ms, jitter: 1ms}"), "links[0].jitter"),
        (
            good.replace("1ms}", "1ms, loss_ppm: 1000001}"),
            "links[0].loss_ppm",
        ),
        (
            good.replace("1ms}", "1ms, loss_ppm: -1}"),
            "links[0].loss_ppm",
        ),
        (
            good.replace("1ms}", "1ms, loss_ppm: 0.5}"),
            "links[0].loss_ppm",
        ),
        (good.replace(", latency: 1ms", ""), "links[0].latency"),
        (good.replace("1ms}", "1h, jitter: 1ns}"), "links[0]"),
    ] {
        let error = parse_link(&entry).unwrap_err().to_string();
        assert!(
            error.contains(&format!("$.x-seedbox.network.{path}")),
            "{error}"
        );
    }
}

#[test]
fn named_volumes_mount_on_one_service_and_reject_binds_and_options() {
    let with = |service: &str, top: &str| {
        BASE.replace("    image:", &format!("{service}\n    image:")) + top
    };
    let sim = parse(&with(
        "    volumes: ['data:/var/lib/data', {type: volume, source: logs, target: /logs}]",
        "volumes:\n  data: {}\n  logs:\n  spare: {}\n",
    ))
    .unwrap();
    let mounts = &sim.services["workload"].volumes;
    assert_eq!(
        mounts
            .iter()
            .map(|m| (m.source.as_str(), m.target.as_str()))
            .collect::<Vec<_>>(),
        [("data", "/var/lib/data"), ("logs", "/logs")]
    );
    let image = ImageDefaults {
        cmd: Some(vec!["app".into()]),
        ..Default::default()
    };
    let launch = resolve_launch(&sim.services["workload"], &image).unwrap();
    assert_eq!(launch.disk_volumes, *mounts);
    // Without volumes the serialized simulation is unchanged.
    assert!(!serde_json::to_string(&parse(BASE).unwrap())
        .unwrap()
        .contains("volumes"));

    let declared = "volumes:\n  data: {}\n";
    for (service, top, path, message) in [
        (
            "    volumes: ['./data:/data']",
            declared,
            "$.services.workload.volumes[0]",
            "unknown volume \"./data\"",
        ),
        (
            "    volumes: ['data:/data:ro']",
            declared,
            "$.services.workload.volumes[0]",
            "access modes",
        ),
        (
            "    volumes: ['other:/data']",
            declared,
            "$.services.workload.volumes[0]",
            "unknown volume",
        ),
        (
            "    volumes: ['data:data']",
            declared,
            "$.services.workload.volumes[0].target",
            "absolute",
        ),
        (
            "    volumes: ['data:/']",
            declared,
            "$.services.workload.volumes[0]",
            "other than /",
        ),
        (
            "    volumes: ['data:/a', 'data:/b']",
            declared,
            "$.services.workload.volumes[1]",
            "already mounted",
        ),
        (
            "    volumes: [{type: bind, source: data, target: /d}]",
            declared,
            "$.services.workload.volumes[0].type",
            "named volumes",
        ),
        (
            "    volumes: [{type: volume, source: data, target: /d, read_only: true}]",
            declared,
            "$.services.workload.volumes[0].read_only",
            "",
        ),
        (
            "    volumes: ['data:/d']",
            "volumes:\n  data: {driver: local}\n",
            "$.volumes.data.driver",
            "",
        ),
        (
            "    volumes: ['data:/d']",
            "volumes:\n  'bad name': {}\n",
            "$.volumes",
            "invalid volume name",
        ),
    ] {
        let error = parse(&with(service, top)).unwrap_err().to_string();
        assert!(
            error.contains(path) && error.contains(message),
            "{service}: {error}"
        );
    }
    let two = BASE.replace(
        "services:\n  workload:\n    image: example/workload:v1\n",
        "services:\n  workload:\n    image: example/workload:v1\n    volumes: ['data:/d']\n  other:\n    image: example/workload:v1\n    volumes: ['data:/d']\n",
    ) + declared;
    assert!(parse(&two)
        .unwrap_err()
        .to_string()
        .contains("each volume lives on one machine's disk"));
}
