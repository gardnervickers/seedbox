use super::*;
use crate::store::ContentDigest;

fn artifact(root: &std::path::Path, name: &str, bytes: &[u8]) -> wire::Artifact {
    let path = root.join(name);
    std::fs::write(&path, bytes).unwrap();
    wire::Artifact {
        sha256: ContentDigest::of(bytes).hex().into(),
        length: bytes.len() as u64,
        artifact: path.to_str().unwrap().into(),
    }
}
fn prepared(root: &std::path::Path) -> wire::Prepared {
    let archive = tar::Builder::new(Vec::new()).into_inner().unwrap();
    let filesystem = artifact(root, "filesystem", &archive);
    let machine = |name: &str| wire::Machine {
        name: name.into(),
        memory_bytes: 128 << 20,
        filesystem: filesystem.clone(),
        process: wire::Process {
            argv: vec![format!("/{name}")],
            environment: Default::default(),
            working_directory: "/".into(),
            user: "0:0".into(),
        },
        disk: None,
        interface: None,
        allow_crashes: false,
    };
    wire::Prepared {
        inventory: wire::Inventory::Tcg {
            policy: None,
            executable: filesystem.clone(),
            kernel: filesystem.clone(),
            init: filesystem.clone(),
            command_line: filesystem.clone(),
            firmware: filesystem.clone(),
            runtime: filesystem.clone(),
            formatter: None,
            environments: Default::default(),
            boot_archives: Default::default(),
            epoch_seconds: 86400,
        },
        oracle: machine("z"),
        machines: vec![machine("a"), machine("b")],
        spawned: ["later-z", "later-a"]
            .map(|name| wire::Spawned {
                spec: format!("{{\"image\":\"{name}\"}}"),
                machine: machine(name),
            })
            .into(),
        quantum_ns: 1_000_000,
        network: Default::default(),
    }
}

#[test]
fn exact_definition_contents_and_roles_keep_original_order() {
    let root = tempfile::tempdir().unwrap();
    let mut input = prepared(root.path());
    input.machines[0].disk = Some(wire::Disk::Image {
        contents: artifact(root.path(), "disk", &[7; 1024]),
    });
    input.spawned[0].machine.disk = Some(wire::Disk::Zero { bytes: 512 });
    input.spawned[1].machine.allow_crashes = true;
    let expected = (
        super::super::definition(&input.oracle).unwrap(),
        input
            .machines
            .iter()
            .map(super::super::definition)
            .collect::<Result<Vec<_>>>()
            .unwrap(),
        input
            .spawned
            .iter()
            .map(|spawned| SpawnDefinition {
                spec: spawned.spec.clone(),
                definition: super::super::definition(&spawned.machine).unwrap(),
                allow_crashes: spawned.machine.allow_crashes,
            })
            .collect::<Vec<_>>(),
    );
    assert_eq!(definitions(&input).unwrap(), expected);
}

#[test]
fn unchanged_length_archive_and_disk_validation_rejects_original_first_bad_input() {
    let root = tempfile::tempdir().unwrap();
    let original = prepared(root.path());
    let mut invalid = original.clone();
    invalid.machines[0].filesystem = artifact(root.path(), "bad-tar", b"not a tar archive");
    invalid.machines[1].disk = Some(wire::Disk::Image {
        contents: artifact(root.path(), "bad-disk", &[0; 513]),
    });
    let mut cases = vec![invalid.clone()];
    invalid.machines[0] = original.machines[0].clone();
    cases.push(invalid.clone());
    invalid.machines[1].disk = Some(wire::Disk::Zero { bytes: 0 });
    cases.push(invalid);
    let mut bad_length = original;
    bad_length.oracle.filesystem.length += 1;
    cases.push(bad_length);
    for input in cases {
        let expected = std::iter::once(&input.oracle)
            .chain(&input.machines)
            .chain(input.spawned.iter().map(|spawned| &spawned.machine))
            .map(super::super::definition)
            .collect::<Result<Vec<_>>>()
            .unwrap_err()
            .to_string();
        assert_eq!(definitions(&input).unwrap_err().to_string(), expected);
    }
}
