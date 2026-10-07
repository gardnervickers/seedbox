//! Finished-image authoring shares Compose validation with the legacy parser.
use seedbox::{
    config::{self, ImageSource},
    oci::GuestPlatform,
    Error,
};
use std::{collections::BTreeMap, fs, path::Path};

const BASE: &str = "name: native-test\nservices:\n  app:\n    image: example/app:v1\nx-seedbox:\n  version: 1\n  seed: 17\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n";
fn native(yaml: &str, platform: GuestPlatform) -> seedbox::Result<config::Simulation> {
    config::parse_native(
        yaml,
        Path::new("/native-fixture/compose.yaml"),
        &BTreeMap::new(),
        platform,
    )
}
fn legacy(yaml: &str) -> seedbox::Result<config::Simulation> {
    config::parse(
        yaml,
        Path::new("/native-fixture/compose.yaml"),
        &BTreeMap::new(),
    )
}

#[test]
fn caller_selects_guest_platform_without_host_inference_or_new_fields() {
    for platform in [GuestPlatform::LinuxAmd64, GuestPlatform::LinuxArm64] {
        let simulation = native(BASE, platform).unwrap();
        assert_eq!(simulation.services["app"].platform, platform.as_str());
        assert_eq!(
            simulation.services["app"].source,
            ImageSource::Registry {
                reference: "example/app:v1".into()
            }
        );
        let encoded = serde_json::to_vec(&simulation).unwrap();
        assert_eq!(
            serde_json::from_slice::<config::Simulation>(&encoded).unwrap(),
            simulation
        );
    }
    assert_eq!(
        serde_json::to_vec(&native(BASE, GuestPlatform::LinuxAmd64).unwrap()).unwrap(),
        serde_json::to_vec(&legacy(BASE).unwrap()).unwrap()
    );
    assert_eq!(
        legacy(BASE).unwrap().services["app"].platform,
        "linux/amd64"
    );
}

#[test]
fn explicit_platform_must_match_selected_guest() {
    for platform in [GuestPlatform::LinuxAmd64, GuestPlatform::LinuxArm64] {
        let yaml = BASE.replace(
            "    image:",
            &format!("    platform: {}\n    image:", platform.as_str()),
        );
        assert_eq!(
            native(&yaml, platform).unwrap().services["app"].platform,
            platform.as_str()
        );
        let other = if platform == GuestPlatform::LinuxArm64 {
            GuestPlatform::LinuxAmd64
        } else {
            GuestPlatform::LinuxArm64
        };
        let Error::Validation { path, message } = native(&yaml, other).unwrap_err() else {
            panic!("expected platform validation");
        };
        assert_eq!(path, "$.services.app.platform");
        assert_eq!(
            message,
            format!(
                "expected {} for the selected guest platform, found {}",
                other.as_str(),
                platform.as_str()
            )
        );
    }
    let yaml = BASE.replace("    image:", "    platform: linux/arm64\n    image:");
    assert_eq!(
        legacy(&yaml).unwrap_err().to_string(),
        "$.services.app.platform: only linux/amd64 is supported"
    );
}

#[test]
fn image_wins_over_externally_owned_build_options_without_local_paths() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("compose.yaml");
    for build in [
        "    build: ./missing-context\n",
        "    build:\n      context: ./missing-context\n      dockerfile: Missingfile\n      args: {EXTERNAL: '${EXTERNAL}'}\n      target: production\n      additional_contexts: {dependency: 'service:builder'}\n      ssh: [default]\n      future_builder_option: true\n",
        "    build: {context: '${MISSING:?external only}', args: {TOKEN: '${SECRET}'}, unsupported_external_option: true}\n",
        "    build: null\n",
    ] {
        let yaml = BASE.replace("    image:", &format!("{build}    image:"));
        let simulation = config::parse_native(&yaml, &source, &BTreeMap::new(), GuestPlatform::LinuxArm64).unwrap();
        assert_eq!(simulation.services["app"].source, ImageSource::Registry { reference: "example/app:v1".into() });
        assert_eq!(simulation.services["app"].platform, "linux/arm64");
        assert!(!root.path().join("missing-context").exists());
        assert_eq!(legacy(&yaml).unwrap_err().to_string(), "$.services.app: specify exactly one of image or build; pull/build precedence is unsupported");
    }
}

#[test]
fn build_only_refusal_explains_external_build_and_image_supply() {
    for build in [
        "./missing-context",
        "{context: ./missing-context, target: production}",
        "null",
    ] {
        let yaml = BASE.replace("    image: example/app:v1", &format!("    build: {build}"));
        let Error::Validation { path, message } =
            native(&yaml, GuestPlatform::LinuxArm64).unwrap_err()
        else {
            panic!("expected authoring validation");
        };
        assert_eq!(path, "$.services.app.build");
        assert_eq!(message, "Seedbox consumes finished images and does not build Dockerfiles; build externally and supply an image: reference");
    }
    let missing = BASE.replace("    image: example/app:v1\n", "    command: [app]\n");
    assert_eq!(
        native(&missing, GuestPlatform::LinuxArm64)
            .unwrap_err()
            .to_string(),
        "$.services.app.image: specify image: with a finished image reference"
    );
    assert_eq!(legacy(&missing).unwrap_err().to_string(), "$.services.app: specify exactly one of image or build; pull/build precedence is unsupported");
}

#[test]
fn legacy_build_context_resolution_and_validation_remain_intact() {
    let root = tempfile::tempdir().unwrap();
    let context = root.path().join("context");
    fs::create_dir(&context).unwrap();
    let dockerfile = context.join("Dockerfile");
    fs::write(&dockerfile, "FROM scratch\n").unwrap();
    let source = root.path().join("compose.yaml");
    for build in ["./context", "{context: ./context, dockerfile: Dockerfile}"] {
        let yaml = BASE.replace("    image: example/app:v1", &format!("    build: {build}"));
        let simulation = config::parse(&yaml, &source, &BTreeMap::new()).unwrap();
        assert_eq!(
            simulation.services["app"].source,
            ImageSource::Build {
                context: context.canonicalize().unwrap(),
                dockerfile: dockerfile.canonicalize().unwrap()
            }
        );
        assert_eq!(simulation.services["app"].platform, "linux/amd64");
    }
    let yaml = BASE.replace(
        "    image: example/app:v1",
        "    build: {context: ./context, target: production}",
    );
    assert_eq!(
        config::parse(&yaml, &source, &BTreeMap::new())
            .unwrap_err()
            .to_string(),
        "$.services.app.build.target: unsupported field"
    );
    let yaml = BASE.replace("    image: example/app:v1", "    build: ./missing-context");
    let expected = fs::canonicalize(root.path().join("missing-context"))
        .unwrap_err()
        .to_string();
    assert_eq!(
        config::parse(&yaml, &source, &BTreeMap::new())
            .unwrap_err()
            .to_string(),
        format!("$.services.app.build: {expected}")
    );
}

#[test]
fn native_images_keep_legacy_scalar_reference_and_interpolation_errors() {
    for (image, expected) in [
        ("''", "expected a nonempty string"),
        ("null", "expected a string (quote numbers and booleans)"),
        ("123", "expected a string (quote numbers and booleans)"),
        ("true", "expected a string (quote numbers and booleans)"),
        (
            "'bad image'",
            "expected a registry image reference without a transport prefix",
        ),
        (
            "'docker://example/app'",
            "expected a registry image reference without a transport prefix",
        ),
        (
            "'${IMAGE}'",
            "Compose interpolation is unsupported; use literal values or $$",
        ),
        ("\"bad\\0image\"", "NUL is not allowed"),
    ] {
        let yaml = BASE.replace("example/app:v1", image);
        let expected = format!("$.services.app.image: {expected}");
        assert_eq!(legacy(&yaml).unwrap_err().to_string(), expected);
        assert_eq!(
            native(&yaml, GuestPlatform::LinuxArm64)
                .unwrap_err()
                .to_string(),
            expected
        );
        let with_build = yaml.replace(
            "    image:",
            "    build: {unrecognized: external}\n    image:",
        );
        assert_eq!(
            native(&with_build, GuestPlatform::LinuxArm64)
                .unwrap_err()
                .to_string(),
            expected
        );
    }
}

#[test]
fn native_mode_keeps_shared_validation_errors_outside_image_and_platform() {
    for yaml in [
        BASE.replace("    image:", "    ports: ['80:80']\n    image:"),
        BASE.replace("    image:", "    command: !custom [hello]\n    image:"),
        BASE.replace("    image:", "    command: 'echo $HOME'\n    image:"),
        BASE.replace("    image:", "    environment: {NUMBER: 123}\n    image:"),
        BASE.replace("  version: 1", "  version: 1\n  version: 2"),
        BASE.replace("  version: 1", "  version: 2"),
        BASE.replace("simulated_time: 1s", "simulated_time: 0s"),
        BASE.replace("service: app", "service: missing"),
        BASE.replace("    image:", "    working_dir: ../outside\n    image:"),
        BASE.replace("  version: 1", "  version: 1\n  faults: [{at: 1s}]"),
    ] {
        assert_eq!(
            native(&yaml, GuestPlatform::LinuxAmd64)
                .unwrap_err()
                .to_string(),
            legacy(&yaml).unwrap_err().to_string(),
            "{yaml}"
        );
    }
}

#[test]
fn native_load_uses_supplied_environment_and_canonical_authoring_path() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("compose.yaml");
    let yaml = BASE.replace(
        "    image:",
        "    environment: [FROM_CALLER, ABSENT]\n    build: ./absent-external-context\n    image:",
    );
    fs::write(&source, &yaml).unwrap();
    let environment = BTreeMap::from([("FROM_CALLER".into(), "provided literally $HOME".into())]);
    let simulation = config::load_native(&source, &environment, GuestPlatform::LinuxArm64).unwrap();
    assert_eq!(simulation.source, source.canonicalize().unwrap());
    assert_eq!(
        simulation.services["app"].environment,
        BTreeMap::from([
            (
                "FROM_CALLER".into(),
                Some("provided literally $HOME".into())
            ),
            ("ABSENT".into(), None)
        ])
    );
    assert_eq!(
        simulation,
        config::parse_native(
            &yaml,
            &simulation.source,
            &environment,
            GuestPlatform::LinuxArm64
        )
        .unwrap()
    );
    assert_eq!(
        config::load_native(
            &root.path().join("missing.yaml"),
            &environment,
            GuestPlatform::LinuxArm64
        )
        .unwrap_err()
        .to_string(),
        config::load(&root.path().join("missing.yaml"), &environment)
            .unwrap_err()
            .to_string()
    );
}
