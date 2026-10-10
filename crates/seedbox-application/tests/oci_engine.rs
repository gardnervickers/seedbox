use seedbox_application::{config, store::ContentStore};
use seedbox_application::{
    container_images::ContainerTransport,
    execution::CancellationToken,
    guest::Architecture,
    oci::{ImageTransport, ImportedImage, RootFilesystem},
};
use std::{collections::BTreeMap, fs, path::Path};
/// Connect the way the CLI does: DOCKER_HOST when set, otherwise Podman's
/// rootless and system sockets before Docker's.
fn engine() -> bollard::Docker {
    if std::env::var_os("DOCKER_HOST").is_some() {
        bollard::Docker::connect_with_defaults()
    } else {
        bollard::Docker::connect_with_podman_defaults()
    }
    .unwrap()
}
fn import(service: &config::Service, directory: &Path, store: &ContentStore) -> ImportedImage {
    ContainerTransport {
        docker: engine(),
        cancellation: CancellationToken::default(),
        progress: |line| eprintln!("{line}"),
        export_cache: None,
    }
    .import(
        service,
        directory,
        store,
        None,
        if cfg!(target_arch = "aarch64") {
            Architecture::Arm64
        } else {
            Architecture::Amd64
        },
    )
    .unwrap()
}
fn entries(image: &ImportedImage) -> BTreeMap<std::path::PathBuf, (tar::EntryType, Vec<u8>)> {
    use std::io::Read;
    let RootFilesystem::Archive(path) = &image.rootfs else {
        panic!("expected export archive")
    };
    tar::Archive::new(fs::File::open(path).unwrap())
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let kind = entry.header().entry_type();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            (path, (kind, data))
        })
        .collect()
}
#[test]
fn registry_images_export_the_expected_files() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    for (name, reference, binary) in [
        (
            "redis",
            "docker.io/library/redis:7.2",
            "rootfs/usr/local/bin/redis-server",
        ),
        (
            "etcd",
            "quay.io/coreos/etcd:v3.5.2",
            "rootfs/usr/local/bin/etcd",
        ),
    ] {
        let image = import(
            &simulation(reference).services["app"],
            &directory.path().join(name),
            &store,
        );
        assert!(entries(&image).contains_key(Path::new(binary)));
        for digest in image.image.layers {
            store.verify(&digest).unwrap();
        }
    }
}
#[test]
fn build_honors_ignore_whiteouts_and_launch_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let context = directory.path().join("context");
    fs::create_dir(&context).unwrap();
    fs::write(context.join("Dockerfile"), "FROM docker.io/library/alpine:3.22\nCOPY . /source\nCOPY resolver /etc/resolv.conf\nRUN test ! -e /source/ignored && mkdir /data && echo old >/data/remove && echo keep >/data/keep\nRUN rm /data/remove && ln -s keep /data/link\nENV MESSAGE=hello\nWORKDIR /data\nUSER 123:456\nENTRYPOINT [\"/bin/echo\"]\nCMD [\"default\"]\n").unwrap();
    fs::write(context.join(".dockerignore"), "ignored\n").unwrap();
    fs::write(context.join("ignored"), "must not upload").unwrap();
    fs::write(context.join("resolver"), "image-owned resolver\n").unwrap();
    let mut service = simulation("unused:test").services.remove("app").unwrap();
    service.source = config::ImageSource::Build {
        context: context.clone(),
        dockerfile: context.join("Dockerfile"),
    };
    service.entrypoint = None;
    service.command = None;
    let store = ContentStore::new(directory.path().join("store"));
    let image = import(&service, &directory.path().join("image"), &store);
    let files = entries(&image);
    assert!(!files.contains_key(Path::new("rootfs/.dockerenv")));
    let again = import(&service, &directory.path().join("again"), &store);
    let (RootFilesystem::Archive(first), RootFilesystem::Archive(second)) =
        (&image.rootfs, &again.rootfs)
    else {
        panic!("expected archives")
    };
    assert_eq!(
        fs::read(first).unwrap(),
        fs::read(second).unwrap(),
        "same image must produce identical rootfs bytes"
    );
    assert!(!files.contains_key(Path::new("rootfs/data/remove")));
    assert!(!files.contains_key(Path::new("rootfs/source/ignored")));
    assert_eq!(files[Path::new("rootfs/data/keep")].1, b"keep\n");
    assert_eq!(
        files[Path::new("rootfs/etc/resolv.conf")].1,
        b"image-owned resolver\n"
    );
    assert!(files[Path::new("rootfs/data/link")].0.is_symlink());
    assert_eq!(image.image.launch.argv, ["/bin/echo", "default"]);
    assert_eq!(image.image.launch.user, "123:456");
    assert_eq!(image.image.launch.working_dir, "/data");
    assert_eq!(image.image.launch.environment["MESSAGE"], "hello");
}
fn simulation(image: &str) -> config::Simulation {
    config::parse(&format!("name: image-test\nservices:\n  app:\n    image: {image}\n    entrypoint: [/bin/sh]\n    command: [-c, 'exit 0']\nx-seedbox:\n  version: 1\n  limits: {{simulated_time: 1s, wall_time: 1m}}\n  check: {{service: app}}\n"), Path::new("/fixture/compose.yaml"), &BTreeMap::new()).unwrap()
}
