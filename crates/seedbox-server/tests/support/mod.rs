use seedbox_api::{InputSource, SimulationSpec, SubmitRequest};

pub fn request() -> SubmitRequest {
    SubmitRequest {
        metadata: Default::default(),
        options: Default::default(),
        request_id: "submit-123".into(),
        fault_profile: None,
        spec: SimulationSpec {
            program: Vec::new(),
            start_paused: false,
            seed: Some(123),
            inputs: InputSource::Project,
            configuration: seedbox_application::config::parse(
                "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
                std::path::Path::new("/project/compose.yaml"),
                &Default::default(),
            )
            .unwrap(),
        },
    }
}

/// A complete input closure for catalogue/transport tests; execution is never launched.
pub fn resolved_request(id: &str) -> SubmitRequest {
    use seedbox_application::{
        execution::{
            Backend, ExecutionProfile, PreparedImage, PreparedInputs, SemanticParameters,
            WitnessMode,
        },
        image::Launch,
        store::ContentDigest,
    };
    let mut request = request();
    request.request_id = id.into();
    request.metadata.name = "test".into();
    let binary = ContentDigest::of(b"catalogue-only-binary");
    let inputs = PreparedInputs {
        machine_memory_mb: Default::default(),
        wall_clock_epoch: Some(86400),
        kernel: binary.clone(),
        images: std::collections::BTreeMap::from([(
            "oracle".into(),
            PreparedImage {
                manifest: binary.clone(),
                config: binary.clone(),
                layers: Vec::new(),
                launch: Launch {
                    argv: vec!["/test".into()],
                    environment: Default::default(),
                    working_dir: "/".into(),
                    user: "0:0".into(),
                    volumes: Vec::new(),
                    disk_volumes: Vec::new(),
                },
            },
        )]),
        placement: std::collections::BTreeMap::from([("oracle".into(), "oracle".into())]),
        guests: std::collections::BTreeMap::from([("oracle".into(), binary.clone())]),
        profile: ExecutionProfile {
            backend: Backend::Other("catalogue-test".into()),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(1024),
        },
        auxiliary: Default::default(),
        provenance: Default::default(),
    };
    let definition = seedbox_application::definition::resolve(
        &request.spec.configuration,
        &inputs,
        123,
        Vec::new(),
    )
    .unwrap();
    request.spec = SimulationSpec::resolved(definition);
    request
}
