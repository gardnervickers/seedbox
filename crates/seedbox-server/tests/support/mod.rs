use seedbox_api::{InputSource, SimulationSpec, SubmitRequest};

pub fn request() -> SubmitRequest {
    SubmitRequest {
        request_id: "submit-123".into(),
        spec: SimulationSpec {
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
