use seedbox_application::{
    config,
    templates::{self, Template},
};
use seedbox_test::process::Guarded;
use std::{collections::BTreeMap, fs, process::Command};

// Type-checks the redis example's standalone workload. #[path] is required:
// that program is built from its own Docker context, not as a workspace crate.
#[allow(dead_code)]
#[path = "../../../examples/redis/workload/main.rs"]
mod redis_workload;

#[test]
fn generated_templates_are_self_contained_and_preserve_the_existing_etcd_checker() {
    let directory = tempfile::tempdir().unwrap();
    for (name, template, services) in [("etcd", Template::Etcd, 4), ("redis", Template::Redis, 2)] {
        let checked_in = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(name)
            .join("seedbox.yaml");
        config::load(&checked_in, &BTreeMap::new()).unwrap();
        let destination = directory.path().join(name);
        templates::init(&destination, template).unwrap();
        let config = config::load(&destination.join("seedbox.yaml"), &BTreeMap::new()).unwrap();
        assert_eq!(config.services.len(), services);
        assert_eq!(config.check.service, "oracle");
        assert_eq!(config.seed, None);
        match &config.services["oracle"].source {
            config::ImageSource::Build {
                context,
                dockerfile,
            } => {
                assert_eq!(
                    *context,
                    destination.canonicalize().unwrap().join("workload")
                );
                assert!(dockerfile.is_file());
            }
            other => panic!("expected runnable workload Dockerfile, got {other:?}"),
        }
    }
    assert_eq!(
        fs::read(directory.path().join("etcd/workload/main.go")).unwrap(),
        include_bytes!("../../../examples/etcd/workload/main.go")
    );
    assert_eq!(
        fs::read(directory.path().join("etcd/workload/go.sum")).unwrap(),
        include_bytes!("../../../examples/etcd/workload/go.sum")
    );
}

#[test]
fn init_cli_refuses_to_overwrite_an_existing_project() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("demo");
    let command = || {
        Command::new(env!("CARGO_BIN_EXE_seedbox"))
            .arg("init")
            .arg(&destination)
            .args(["--template", "redis"])
            .guarded_output()
            .unwrap()
    };
    assert!(command().status.success());
    fs::write(destination.join("seedbox.yaml"), "user edits").unwrap();
    assert!(!command().status.success());
    assert_eq!(
        fs::read_to_string(destination.join("seedbox.yaml")).unwrap(),
        "user edits"
    );
}
