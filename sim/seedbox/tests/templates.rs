use seedbox::{
    config,
    templates::{self, Template},
};
use std::{collections::BTreeMap, fs, process::Command};

#[allow(dead_code)]
#[path = "../templates/redis/main.rs"]
mod redis_workload;

#[test]
fn generated_templates_are_self_contained_and_preserve_the_existing_etcd_checker() {
    let directory = tempfile::tempdir().unwrap();
    for (name, template, services) in [("etcd", Template::Etcd, 4), ("redis", Template::Redis, 2)] {
        let destination = directory.path().join(name);
        templates::init(&destination, template).unwrap();
        let config = config::load(&destination.join("compose.yaml"), &BTreeMap::new()).unwrap();
        assert_eq!(config.services.len(), services);
        assert_eq!(config.check.service, "client");
        assert_eq!(config.seed, Some(1009));
        match &config.services["client"].source {
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
        include_bytes!("../../guest/client/main.go")
    );
    assert_eq!(
        fs::read(directory.path().join("etcd/workload/go.sum")).unwrap(),
        include_bytes!("../../guest/client/go.sum")
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
            .output()
            .unwrap()
    };
    assert!(command().status.success());
    fs::write(destination.join("compose.yaml"), "user edits").unwrap();
    assert!(!command().status.success());
    assert_eq!(
        fs::read_to_string(destination.join("compose.yaml")).unwrap(),
        "user edits"
    );
}
