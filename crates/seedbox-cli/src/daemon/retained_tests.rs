//! Cold retained-provider ownership, not native guest or executable proof.
use super::*;
use clap::Parser;
use seedbox_application::{
    driver_world::{self, DriverBackend, DriverExecutor},
    execution::{ExecutionProfile, SemanticParameters, WitnessMode},
    store::ContentDigest,
};
use seedbox_engine_protocol::driver_world as wire;
use seedbox_test::process::Guarded;
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};

#[test]
fn retained_driver_acquires_only_recorded_runtime_and_owns_it_until_executor_drop() {
    // tar's fork can briefly inherit another test's CLOEXEC flock before exec.
    // Session probes deliberately use a zero-wait ownership check.
    let _scope = super::session_test_scope();
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("package");
    fs::create_dir(&package).unwrap();
    let host = format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    );
    fs::write(package.join("host-system"), host).unwrap();
    let guest = package.join("pv").join(std::env::consts::ARCH);
    fs::create_dir_all(&guest).unwrap();
    let mut elf = vec![0; 20];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    elf[18..20].copy_from_slice(
        &(if cfg!(target_arch = "x86_64") {
            62u16
        } else {
            183
        })
        .to_le_bytes(),
    );
    for (name, bytes) in [
        ("kernel", b"kernel".as_slice()),
        ("init", elf.as_slice()),
        ("command-line", b"".as_slice()),
        ("mke2fs", elf.as_slice()),
        ("progress.tar", b"fixture progress".as_slice()),
    ] {
        let path = guest.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let archive = root.path().join("runtime.tar.gz");
    assert!(std::process::Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .arg("-C")
        .arg(&package)
        .arg("host-system")
        .args(
            ["kernel", "init", "command-line", "mke2fs", "progress.tar"]
                .map(|name| format!("pv/{}/{name}", std::env::consts::ARCH))
        )
        .guarded_status()
        .unwrap()
        .success());
    let store = ContentStore::new(root.path().join("store"));
    let asset = |path: &Path| {
        let bytes = fs::read(path).unwrap();
        wire::Artifact {
            sha256: ContentDigest::of(&bytes).hex().into(),
            length: bytes.len() as u64,
            artifact: path.to_str().unwrap().into(),
        }
    };
    let worker = root.path().join("worker");
    fs::write(&worker, b"cold worker fixture; never executed").unwrap();
    let filesystem = root.path().join("filesystem.tar");
    fs::write(&filesystem, [0; 1024]).unwrap();
    let mut semantic = SemanticParameters::new(256);
    semantic.engine = Some(DriverExecutor::descriptor(
        DriverBackend::Pv,
        std::env::consts::ARCH,
    ));
    let mut inputs = PreparedInputs {
        wall_clock_epoch: Some(86400),
        kernel: store.put_file(&guest.join("kernel")).unwrap(),
        images: BTreeMap::new(),
        guests: BTreeMap::new(),
        placement: BTreeMap::new(),
        profile: ExecutionProfile {
            backend: Backend::Other("driver-pv".into()),
            engine: store.put_file(&worker).unwrap(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: semantic,
        },
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    let prepared = wire::Prepared {
        inventory: wire::Inventory::Pv {
            policy: None,
            executable: asset(&worker),
            kernel: asset(&guest.join("kernel")),
            init: asset(&guest.join("init")),
            command_line: asset(&guest.join("command-line")),
            progress: asset(&guest.join("progress.tar")),
            runtime: asset(&archive),
            formatter: Some(asset(&guest.join("mke2fs"))),
            environments: BTreeMap::new(),
            boot_archives: BTreeMap::new(),
            blocks_per_progress_call: std::num::NonZeroU32::new(32).unwrap(),
            epoch_seconds: 86400,
        },
        oracle: wire::Machine {
            name: "oracle".into(),
            memory_bytes: 256 << 20,
            filesystem: asset(&filesystem),
            process: wire::Process {
                argv: vec!["/app".into()],
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
            disk: None,
            interface: None,
        },
        machines: vec![],
        spawnable: vec![],
        quantum_ns: 10,
        network: wire::Network::default(),
    };
    driver_world::retain_prepared(&mut inputs, &prepared, &store).unwrap();
    let digest = driver_world::runtime_digest(&inputs, &store).unwrap();
    assert_eq!(digest, ContentDigest::of(&fs::read(&archive).unwrap()));
    let session = root.path().join("actual-session");
    fs::create_dir(&session).unwrap();
    let super::super::Command::Serve {
        inputs: arguments, ..
    } = super::super::Cli::try_parse_from(["seedbox", "serve"])
        .unwrap()
        .command
    else {
        panic!("serve parser")
    };
    let provider = NativeBackends {
        inputs: arguments,
        environment: vec![],
        session_lease: session.join("owner.lease"),
        session_root: session.clone(),
    };
    let executor = provider.retained(&inputs, &store).unwrap();
    assert!(
        !executor.runtime_inputs(&inputs).contains(&&digest),
        "the executor must actually retain and bind the acquired runtime, not merely probe it"
    );
    let directories = || {
        fs::read_dir(&session)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("driver-runtime-")
            })
            .collect::<Vec<_>>()
    };
    let owned = directories();
    assert_eq!(owned.len(), 1);
    assert!(owned[0].join("runtime.tar.gz").exists());
    drop(executor);
    assert!(directories().is_empty());

    let corrupt = root.path().join("corrupt-runtime.tar.gz");
    fs::write(&corrupt, b"not a runtime archive").unwrap();
    let mut wrong = prepared;
    let wire::Inventory::Pv { runtime, .. } = &mut wrong.inventory else {
        unreachable!()
    };
    *runtime = asset(&corrupt);
    let mut bad_inputs = inputs;
    driver_world::retain_prepared(&mut bad_inputs, &wrong, &store).unwrap();
    assert!(
        provider.retained(&bad_inputs, &store).is_err(),
        "a corrupt retained archive must not fall back to embedded/default startup assets"
    );
    assert!(
        directories().is_empty(),
        "failed acquisition must reclaim partial owned runtime inputs"
    );
}
