use seedbox_application::{
    config,
    image::{resolve_launch, ImageDefaults},
};
use std::{collections::BTreeMap, path::Path};

const BASE: &str = "name: test\nmachines:\n  workload:\n    image: example/workload:v1\noracle: {image: example/oracle:v1}\n";

fn parse(yaml: &str) -> seedbox_application::Result<config::Simulation> {
    config::parse(yaml, Path::new("/fixture/seedbox.yaml"), &BTreeMap::new())
}

#[test]
fn only_native_project_fields_are_accepted_without_silent_legacy_fallback() {
    for source in [
        "services: {oracle: {image: example/oracle}}",
        "oracle: {image: example/oracle}\nx-seedbox: {version: 1}",
        "oracle: {image: example/oracle}\nsimulator: {backend: tcg}",
        "oracle: {image: example/oracle}\nlimits: {simulated_time: 1s}",
        "oracle: {image: example/oracle}\noracle: {image: duplicate}",
        "oracle: !tag {image: example/oracle}",
        "oracle: {image: example/oracle, environment: [HOME]}",
        "oracle: {image: example/oracle, command: 'echo hello'}",
    ] {
        assert!(parse(source).is_err(), "accepted {source}");
    }
    let project = parse(BASE).unwrap();
    assert_eq!(project.check.service, "oracle");
    assert_eq!(project.control.unwrap().service, "oracle");
    assert_eq!(project.simulator.progress_budget.get(), 10_000);
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
fn durations_are_exact_and_checked_without_floating_point() {
    for (raw, expected) in [
        ("0.000000001s", 1),
        ("1.25ms", 1_250_000),
        ("18446744073709551615ns", u64::MAX),
    ] {
        assert_eq!(config::duration_ns(raw).unwrap(), expected);
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
        assert!(config::duration_ns(raw).is_err(), "accepted {raw}");
    }
}

#[test]
fn image_entrypoint_and_cmd_follow_native_override_rules() {
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
fn native_projects_resolve_memory_per_machine_and_validate_the_architecture_range() {
    let document = "name: mixed\nmachines:\n  app: {image: example/app, memoryMiB: 2048}\noracle: {image: example/oracle, memoryMiB: 256}\n";
    for architecture in [
        seedbox_application::guest::Architecture::Amd64,
        seedbox_application::guest::Architecture::Arm64,
    ] {
        let configuration = config::parse_for_architecture(
            document,
            Path::new("/project/seedbox.yaml"),
            &Default::default(),
            architecture,
        )
        .unwrap();
        assert_eq!(configuration.services["app"].memory_mb, Some(2048));
        assert_eq!(configuration.services["oracle"].memory_mb, Some(256));
        assert_eq!(configuration.machine.memory_mb, 1024);
        for memory in [0, 63, 65537] {
            assert!(config::parse_for_architecture(
                &document.replace("2048", &memory.to_string()),
                Path::new("/project/seedbox.yaml"),
                &Default::default(),
                architecture
            )
            .is_err());
        }
    }
    assert!(config::parse_for_architecture(
        &document.replace("2048", "3073"),
        Path::new("/project/seedbox.yaml"),
        &Default::default(),
        seedbox_application::guest::Architecture::Arm64
    )
    .unwrap_err()
    .to_string()
    .contains("memoryMiB"));
    let defaults = config::parse_for_architecture(
        &document
            .replace(", memoryMiB: 2048", "")
            .replace(", memoryMiB: 256", ""),
        Path::new("/project/seedbox.yaml"),
        &Default::default(),
        seedbox_application::guest::Architecture::Arm64,
    )
    .unwrap();
    assert!(defaults
        .services
        .values()
        .all(|service| service.memory_mb.is_none()));
}

#[test]
fn allow_crashes_reaches_the_submitted_configuration() {
    let document = BASE.replace(
        "    image: example/workload:v1\n",
        "    image: example/workload:v1\n    allowCrashes: true\n",
    );
    let sim = parse(&document).unwrap();
    assert!(sim.services["workload"].allow_crashes);
    assert!(!sim.services["oracle"].allow_crashes);
}
