//! Scenario inputs are validated without preparing images or starting a VM.
use std::{collections::BTreeMap, path::Path};
#[test]
fn every_acceptance_scenario_parses_and_retains_its_guest_inputs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/workloads");
    let mut paths: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    for path in paths {
        for architecture in [
            seedbox_application::guest::Architecture::Amd64,
            seedbox_application::guest::Architecture::Arm64,
        ] {
            let scenario = seedbox_application::config::load_for_architecture(
                &path,
                &BTreeMap::new(),
                architecture,
            )
            .unwrap_or_else(|error| panic!("{}: {architecture:?}: {error}", path.display()));
            assert!(
                scenario.services.contains_key(&scenario.check.service),
                "{}",
                path.display()
            );
        }
    }
}
