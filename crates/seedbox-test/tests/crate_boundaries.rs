// Checks declared local dependency edges, including disabled features, other
// targets, build dependencies and dev dependencies. No guests are started.
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Normal,
    Build,
    Dev,
}

#[derive(Debug)]
struct Package {
    name: String,
    edges: Vec<(PathBuf, Kind)>,
}
type Graph = BTreeMap<PathBuf, Package>;

fn local_graph(manifest: &Path) -> Result<Graph, String> {
    let mut graph = Graph::new();
    let mut pending = VecDeque::from([manifest.to_path_buf()]);
    let mut visited = BTreeSet::new();
    while let Some(manifest) = pending.pop_front() {
        let manifest = manifest
            .canonicalize()
            .map_err(|error| format!("cannot resolve manifest {}: {error}", manifest.display()))?;
        if graph.contains_key(&manifest) || !visited.insert(manifest.clone()) {
            continue;
        }
        // no-deps avoids registry resolution; recursively inspect every local
        // path edge as well, so excluded standalone workspaces cannot hide an
        // indirect forbidden dependency. Do not filter features or platforms.
        let output = Command::new(env!("CARGO"))
            .args([
                "metadata",
                "--locked",
                "--offline",
                "--no-deps",
                "--format-version=1",
                "--manifest-path",
            ])
            .arg(&manifest)
            .output()
            .map_err(|error| format!("metadata {}: {error}", manifest.display()))?;
        if !output.status.success() {
            return Err(format!(
                "metadata {} failed ({}): {}",
                manifest.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let metadata: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("metadata {}: {error}", manifest.display()))?;
        for package in metadata["packages"].as_array().ok_or("missing packages")? {
            let name = package["name"].as_str().ok_or("missing package name")?;
            let path = Path::new(
                package["manifest_path"]
                    .as_str()
                    .ok_or("missing package manifest")?,
            )
            .canonicalize()
            .map_err(|error| format!("{name} manifest: {error}"))?;
            let mut edges = Vec::new();
            for dependency in package["dependencies"]
                .as_array()
                .ok_or("missing dependencies")?
            {
                let Some(directory) = dependency["path"].as_str() else {
                    continue;
                };
                let destination = Path::new(directory)
                    .join("Cargo.toml")
                    .canonicalize()
                    .map_err(|error| format!("{name} dependency {directory}: {error}"))?;
                let kind = match dependency["kind"].as_str() {
                    None => Kind::Normal,
                    Some("build") => Kind::Build,
                    Some("dev") => Kind::Dev,
                    Some(other) => return Err(format!("unknown dependency kind {other}")),
                };
                pending.push_back(destination.clone());
                edges.push((destination, kind));
            }
            graph.insert(
                path,
                Package {
                    name: name.to_owned(),
                    edges,
                },
            );
        }
    }
    Ok(graph)
}

fn named<'a>(graph: &'a Graph, name: &str) -> Result<&'a PathBuf, String> {
    let matches: Vec<_> = graph
        .iter()
        .filter(|(_, package)| package.name == name)
        .map(|(path, _)| path)
        .collect();
    match matches.as_slice() {
        [path] => Ok(path),
        _ => Err(format!(
            "expected one local {name}, found {}",
            matches.len()
        )),
    }
}

fn verify(graph: &Graph) -> Result<(), String> {
    let common = named(graph, "seedbox-machine")?;
    let pv = named(graph, "seedbox-machine-pv")?;
    let tcg = named(graph, "seedbox-machine-tcg")?;
    let driver = named(graph, "seedbox-driver")?;
    for (source, target) in [
        (driver, common),
        (driver, pv),
        (driver, tcg),
        (pv, common),
        (tcg, common),
    ] {
        if !graph[source]
            .edges
            .iter()
            .any(|(path, kind)| path == target && *kind == Kind::Normal)
        {
            return Err(format!(
                "{} must declare a normal dependency on {}",
                graph[source].name, graph[target].name
            ));
        }
    }
    for (source, forbidden) in [
        (common, driver),
        (common, pv),
        (common, tcg),
        (pv, driver),
        (tcg, driver),
    ] {
        let mut pending = VecDeque::from([(source.clone(), graph[source].name.clone())]);
        let mut visited = BTreeSet::new();
        while let Some((path, trace)) = pending.pop_front() {
            if &path == forbidden {
                return Err(format!("forbidden dependency path: {trace}"));
            }
            if !visited.insert(path.clone()) {
                continue;
            }
            let package = graph
                .get(&path)
                .ok_or_else(|| format!("uninspected local dependency {}", path.display()))?;
            for (next, kind) in &package.edges {
                let next_package = graph
                    .get(next)
                    .ok_or_else(|| format!("uninspected local dependency {}", next.display()))?;
                pending.push_back((
                    next.clone(),
                    format!("{trace} --{kind:?}--> {}", next_package.name),
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn repository_dependency_direction() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let graph = local_graph(&manifest).unwrap_or_else(|error| panic!("{error}"));
    verify(&graph).unwrap_or_else(|error| panic!("{error}"));
}

fn fixture() -> Graph {
    let names = [
        "seedbox-machine",
        "seedbox-machine-pv",
        "seedbox-machine-tcg",
        "seedbox-driver",
        "outside-workspace-helper",
    ];
    let mut graph: Graph = names
        .into_iter()
        .map(|name| {
            (
                PathBuf::from(name),
                Package {
                    name: name.into(),
                    edges: Vec::new(),
                },
            )
        })
        .collect();
    for (source, target) in [
        ("seedbox-driver", "seedbox-machine"),
        ("seedbox-driver", "seedbox-machine-pv"),
        ("seedbox-driver", "seedbox-machine-tcg"),
        ("seedbox-machine-pv", "seedbox-machine"),
        ("seedbox-machine-tcg", "seedbox-machine"),
    ] {
        graph
            .get_mut(Path::new(source))
            .unwrap()
            .edges
            .push((PathBuf::from(target), Kind::Normal));
    }
    graph
}

#[test]
fn policy_accepts_the_required_graph() {
    verify(&fixture()).unwrap();
}

#[test]
fn policy_requires_all_four_packages() {
    for name in [
        "seedbox-machine",
        "seedbox-machine-pv",
        "seedbox-machine-tcg",
        "seedbox-driver",
    ] {
        let mut graph = fixture();
        graph.remove(Path::new(name));
        assert!(verify(&graph).unwrap_err().contains(name));
    }
}

#[test]
fn policy_rejects_reverse_edges_of_every_kind() {
    for source in [
        "seedbox-machine",
        "seedbox-machine-pv",
        "seedbox-machine-tcg",
    ] {
        for kind in [Kind::Normal, Kind::Build, Kind::Dev] {
            let mut graph = fixture();
            graph
                .get_mut(Path::new(source))
                .unwrap()
                .edges
                .push((PathBuf::from("seedbox-driver"), kind));
            let error = verify(&graph).unwrap_err();
            assert!(error.contains("forbidden dependency path"), "{error}");
            assert!(error.contains(&format!("--{kind:?}-->")), "{error}");
        }
    }
}

#[test]
fn policy_rejects_indirect_dev_dependency() {
    let mut graph = fixture();
    graph
        .get_mut(Path::new("seedbox-machine-pv"))
        .unwrap()
        .edges
        .push((PathBuf::from("outside-workspace-helper"), Kind::Dev));
    graph
        .get_mut(Path::new("outside-workspace-helper"))
        .unwrap()
        .edges
        .push((PathBuf::from("seedbox-driver"), Kind::Normal));
    let error = verify(&graph).unwrap_err();
    assert!(error.contains("outside-workspace-helper --Normal--> seedbox-driver"));
}

#[test]
fn policy_common_cannot_instantiate_backend_specs_via_dev_dependencies() {
    for target in ["seedbox-machine-pv", "seedbox-machine-tcg"] {
        let mut graph = fixture();
        graph
            .get_mut(Path::new("seedbox-machine"))
            .unwrap()
            .edges
            .push((PathBuf::from(target), Kind::Dev));
        assert!(verify(&graph).unwrap_err().contains(target));
    }
}

#[test]
fn policy_required_edges_cannot_be_dev_only() {
    let mut graph = fixture();
    graph.get_mut(Path::new("seedbox-driver")).unwrap().edges[0].1 = Kind::Dev;
    assert!(verify(&graph).unwrap_err().contains("normal dependency"));
}
