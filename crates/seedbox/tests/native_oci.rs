//! Portable import fixtures use only completed image bytes and never unpack image paths.
use seedbox::{
    config::{self, Service},
    execution::CancellationToken,
    guest::build_initramfs_image,
    oci::{
        filesystem::ImageFilesystem,
        native::{ImageInput, NativeImportedImage, NativeOci, RegistryClient, RegistryReference},
        GuestPlatform,
    },
    preparation::PreparationContext,
    store::{ContentDigest, ContentStore},
    Error,
};
use serde_json::{json, Value};
use std::{
    cell::Cell,
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const RAW: &str = "application/vnd.oci.image.layer.v1.tar";
const GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

fn service() -> Service {
    config::parse(
        "name: native-test\nservices:\n  app:\n    image: fixture:latest\nx-seedbox:\n  version: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n",
        Path::new("/fixture/compose.yaml"),
        &BTreeMap::new(),
    )
    .unwrap()
    .services
    .remove("app")
    .unwrap()
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", ContentDigest::of(bytes).hex())
}

fn descriptor(bytes: &[u8], media: &str) -> Value {
    json!({"mediaType":media,"digest":digest(bytes),"size":bytes.len()})
}

struct Fixture {
    temp: tempfile::TempDir,
    layout: PathBuf,
    store: ContentStore,
}

struct Image {
    descriptor: Value,
    manifest: Vec<u8>,
    config: Vec<u8>,
    layers: Vec<Vec<u8>>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let layout = temp.path().join("layout");
        fs::create_dir_all(layout.join("blobs/sha256")).unwrap();
        fs::write(
            layout.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        let store = ContentStore::new(temp.path().join("store"));
        Self {
            temp,
            layout,
            store,
        }
    }

    fn blob(&self, bytes: &[u8]) {
        fs::write(
            self.layout
                .join("blobs/sha256")
                .join(ContentDigest::of(bytes).hex()),
            bytes,
        )
        .unwrap();
    }

    fn image(&self, architecture: &str, layers: &[(&[u8], &str)], bad_diff: bool) -> Image {
        let config = serde_json::to_vec_pretty(&json!({
            "os":"linux", "architecture":architecture,
            "config":{"Cmd":["/bin/app"],"Env":["KEEP=original"],"User":"123:456"},
            "rootfs":{"type":"layers","diff_ids":layers.iter().map(|(raw,_)| {
                if bad_diff { digest(b"wrong uncompressed layer") } else { digest(raw) }
            }).collect::<Vec<_>>()}
        }))
        .unwrap();
        self.blob(&config);
        let compressed: Vec<_> = layers
            .iter()
            .map(|(raw, media)| match *media {
                GZIP => gzip(raw),
                ZSTD => zstd::stream::encode_all(*raw, 1).unwrap(),
                RAW => raw.to_vec(),
                _ => panic!("fixture compression"),
            })
            .collect();
        for bytes in &compressed {
            self.blob(bytes);
        }
        let manifest = serde_json::to_vec_pretty(&json!({
            "schemaVersion":2,"mediaType":MANIFEST,"config":descriptor(&config,CONFIG),
            "layers":compressed.iter().zip(layers).map(|(bytes,(_,media))| descriptor(bytes,media)).collect::<Vec<_>>()
        })).unwrap();
        self.blob(&manifest);
        let mut entry = descriptor(&manifest, MANIFEST);
        entry["platform"] = json!({"os":"linux","architecture":architecture});
        Image {
            descriptor: entry,
            manifest,
            config,
            layers: compressed,
        }
    }

    fn index(&self, entries: &[Value]) -> Vec<u8> {
        let bytes = serde_json::to_vec_pretty(
            &json!({"schemaVersion":2,"mediaType":INDEX,"manifests":entries}),
        )
        .unwrap();
        fs::write(self.layout.join("index.json"), &bytes).unwrap();
        bytes
    }

    fn import(&self, name: &str, platform: GuestPlatform) -> seedbox::Result<NativeImportedImage> {
        NativeOci::default().import(
            ImageInput::OciLayout {
                path: &self.layout,
                reference: None,
            },
            &service(),
            &self.temp.path().join(name),
            &self.store,
            platform,
            None,
        )
    }

    fn package(&self, tree: &ImageFilesystem, name: &str) -> BTreeMap<String, CpioEntry> {
        let helper = self.temp.path().join("helper");
        fs::write(&helper, b"fixture helper").unwrap();
        let output = self.temp.path().join(name);
        build_initramfs_image(tree, &helper, &helper, None, b"recipe", &output, &|| Ok(()))
            .unwrap();
        cpio_entries(&fs::read(output).unwrap())
    }
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

struct TarEntry<'a> {
    name: &'a str,
    data: &'a [u8],
    kind: tar::EntryType,
    link: Option<&'a str>,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
}

impl<'a> TarEntry<'a> {
    fn file(name: &'a str, data: &'a [u8]) -> Self {
        Self {
            name,
            data,
            kind: tar::EntryType::Regular,
            link: None,
            mode: 0o640,
            uid: 123,
            gid: 456,
            mtime: 123456789,
        }
    }
    fn link(name: &'a str, target: &'a str, hard: bool) -> Self {
        Self {
            kind: if hard {
                tar::EntryType::Link
            } else {
                tar::EntryType::Symlink
            },
            link: Some(target),
            ..Self::file(name, b"")
        }
    }
}

fn header(entry: &TarEntry<'_>) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    // Directly set the name so hostile fixtures are not sanitized by the builder.
    assert!(entry.name.len() < 100);
    header.as_mut_bytes()[..entry.name.len()].copy_from_slice(entry.name.as_bytes());
    header.set_entry_type(entry.kind);
    header.set_size(entry.data.len() as u64);
    header.set_mode(entry.mode);
    header.set_uid(entry.uid);
    header.set_gid(entry.gid);
    header.set_mtime(entry.mtime);
    if let Some(link) = entry.link {
        header.set_link_name(link).unwrap();
    }
    header.set_cksum();
    header
}

fn tar_bytes(entries: &[TarEntry<'_>]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        builder.append(&header(entry), entry.data).unwrap();
    }
    builder.into_inner().unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct CpioEntry {
    inode: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u32,
    mtime: u32,
    data: Vec<u8>,
}

// Read the published newc format rather than relying on host extraction/chown.
fn cpio_entries(bytes: &[u8]) -> BTreeMap<String, CpioEntry> {
    let mut entries = BTreeMap::new();
    let mut offset = 0;
    loop {
        assert_eq!(&bytes[offset..offset + 6], b"070701");
        let fields: Vec<_> = (0..13)
            .map(|i| {
                u32::from_str_radix(
                    std::str::from_utf8(&bytes[offset + 6 + i * 8..offset + 14 + i * 8]).unwrap(),
                    16,
                )
                .unwrap()
            })
            .collect();
        let name_size = fields[11] as usize;
        let name_start = offset + 110;
        assert_eq!(bytes[name_start + name_size - 1], 0);
        let name = std::str::from_utf8(&bytes[name_start..name_start + name_size - 1])
            .unwrap()
            .to_owned();
        let data_start = (name_start + name_size + 3) & !3;
        let data_end = data_start + fields[6] as usize;
        offset = (data_end + 3) & !3;
        if name == "TRAILER!!!" {
            break;
        }
        let entry = CpioEntry {
            inode: fields[0],
            mode: fields[1],
            uid: fields[2],
            gid: fields[3],
            links: fields[4],
            mtime: fields[5],
            data: bytes[data_start..data_end].to_vec(),
        };
        assert!(entries.insert(name, entry).is_none());
    }
    entries
}

#[test]
fn multi_architecture_index_retains_original_bytes_for_all_compressions() {
    for media in [RAW, GZIP, ZSTD] {
        let fixture = Fixture::new();
        let amd_tar = tar_bytes(&[TarEntry::file("architecture", b"amd64")]);
        let arm_tar = tar_bytes(&[TarEntry::file("architecture", b"arm64")]);
        let amd = fixture.image("amd64", &[(&amd_tar, media)], false);
        let mut arm = fixture.image("arm64", &[(&arm_tar, media)], false);
        arm.descriptor["platform"]["variant"] = json!("v8");
        let index = fixture.index(&[arm.descriptor.clone(), amd.descriptor.clone()]);
        for (name, platform, image) in [
            ("amd", GuestPlatform::LinuxAmd64, &amd),
            ("arm", GuestPlatform::LinuxArm64, &arm),
        ] {
            let imported = fixture.import(name, platform).unwrap();
            assert_eq!(
                fixture.store.read(&imported.image.manifest).unwrap(),
                image.manifest
            );
            assert_eq!(
                fixture.store.read(&imported.image.config).unwrap(),
                image.config
            );
            assert_eq!(
                fixture.store.read(&imported.image.layers[0]).unwrap(),
                image.layers[0]
            );
            assert_eq!(
                fixture
                    .store
                    .read(&imported.auxiliary["source-index"])
                    .unwrap(),
                index
            );
            assert_eq!(imported.image.launch.argv, ["/bin/app"]);
            let packaged = fixture.package(&imported.filesystem, &format!("{name}.cpio"));
            assert_eq!(
                packaged["rootfs/architecture"].data,
                platform.architecture().as_bytes()
            );
        }
    }
}

#[test]
fn unlabelled_index_leaves_select_by_config_and_amd64_baseline_variant_is_accepted() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[]);
    let mut amd = fixture.image("amd64", &[(&raw, RAW)], false);
    let mut arm = fixture.image("arm64", &[(&raw, RAW)], false);
    amd.descriptor.as_object_mut().unwrap().remove("platform");
    arm.descriptor.as_object_mut().unwrap().remove("platform");
    fixture.index(&[arm.descriptor, amd.descriptor.clone()]);
    assert_eq!(
        fixture
            .import("unlabelled", GuestPlatform::LinuxAmd64)
            .unwrap()
            .image
            .manifest,
        ContentDigest::of(&amd.manifest)
    );
    amd.descriptor["platform"] = json!({"os":"linux","architecture":"amd64","variant":"v1"});
    fixture.index(&[amd.descriptor]);
    fixture
        .import("baseline", GuestPlatform::LinuxAmd64)
        .unwrap();
}

#[test]
fn repeated_descriptors_and_tag_aliases_for_one_leaf_are_not_ambiguous() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[TarEntry::file("app", b"shared tagged content")]);
    let image = fixture.image("amd64", &[(&raw, GZIP)], false);
    let mut first_tag = image.descriptor.clone();
    first_tag["annotations"] = json!({"org.opencontainers.image.ref.name":"stable"});
    let mut second_tag = image.descriptor.clone();
    second_tag["annotations"] = json!({"org.opencontainers.image.ref.name":"latest"});
    let nested_index = serde_json::to_vec(&json!({
        "schemaVersion":2,"mediaType":INDEX,"manifests":[first_tag.clone(),second_tag.clone()]
    }))
    .unwrap();
    fixture.blob(&nested_index);
    let nested = descriptor(&nested_index, INDEX);
    let index = fixture.index(&[
        first_tag.clone(),
        second_tag.clone(),
        first_tag.clone(),
        nested.clone(),
    ]);
    let imported = fixture
        .import("aliased", GuestPlatform::LinuxAmd64)
        .unwrap();
    assert_eq!(imported.image.manifest, ContentDigest::of(&image.manifest));
    assert_eq!(
        fixture.store.read(&imported.image.manifest).unwrap(),
        image.manifest
    );
    assert_eq!(
        fixture
            .store
            .read(&imported.auxiliary["source-index"])
            .unwrap(),
        index
    );
    assert_eq!(
        fixture.package(&imported.filesystem, "aliased.cpio")["rootfs/app"].data,
        b"shared tagged content"
    );

    // Repeated aliases must not hide a later genuinely distinct matching leaf.
    let other_raw = tar_bytes(&[TarEntry::file("app", b"different tagged content")]);
    let other = fixture.image("amd64", &[(&other_raw, GZIP)], false);
    fixture.index(&[first_tag, second_tag, nested, other.descriptor]);
    let error = fixture
        .import("distinct", GuestPlatform::LinuxAmd64)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("2 matching linux/amd64"),
        "{error}"
    );
}

#[test]
fn ambiguous_index_requires_a_reference_and_reference_selects_exact_manifest() {
    let fixture = Fixture::new();
    let a = tar_bytes(&[TarEntry::file("app", b"a")]);
    let b = tar_bytes(&[TarEntry::file("app", b"b")]);
    let mut first = fixture.image("amd64", &[(&a, RAW)], false);
    let mut second = fixture.image("amd64", &[(&b, RAW)], false);
    first.descriptor["annotations"] = json!({"org.opencontainers.image.ref.name":"first"});
    second.descriptor["annotations"] = json!({"org.opencontainers.image.ref.name":"second"});
    fixture.index(&[first.descriptor, second.descriptor]);
    assert!(fixture
        .import("ambiguous", GuestPlatform::LinuxAmd64)
        .err()
        .unwrap()
        .to_string()
        .contains("2 matching"));
    let imported = NativeOci::default()
        .import(
            ImageInput::OciLayout {
                path: &fixture.layout,
                reference: Some("second"),
            },
            &service(),
            &fixture.temp.path().join("selected"),
            &fixture.store,
            GuestPlatform::LinuxAmd64,
            None,
        )
        .unwrap();
    assert_eq!(imported.image.manifest, ContentDigest::of(&second.manifest));
}

#[test]
fn config_platform_mismatch_is_rejected_before_layer_access_or_unpack() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[TarEntry::file("must-not-extract", b"data")]);
    let mut image = fixture.image("arm64", &[(&raw, RAW)], false);
    image.descriptor["platform"]["architecture"] = json!("amd64");
    fixture.index(&[image.descriptor]);
    fs::remove_file(
        fixture
            .layout
            .join("blobs/sha256")
            .join(ContentDigest::of(&raw).hex()),
    )
    .unwrap();
    let error = fixture
        .import("rejected", GuestPlatform::LinuxAmd64)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("0 matching linux/amd64"),
        "{error}"
    );
    assert!(!fixture.temp.path().join("rejected/files").exists());
}

#[test]
fn equal_size_descriptor_corruption_is_rejected_for_manifest_config_and_layer() {
    for object in ["manifest", "config", "layer"] {
        let fixture = Fixture::new();
        let raw = tar_bytes(&[TarEntry::file("data", b"original")]);
        let image = fixture.image("amd64", &[(&raw, GZIP)], false);
        fixture.index(&[image.descriptor]);
        let original = match object {
            "manifest" => &image.manifest,
            "config" => &image.config,
            _ => &image.layers[0],
        };
        let mut corrupt = original.clone();
        corrupt[0] ^= 1;
        fs::write(
            fixture
                .layout
                .join("blobs/sha256")
                .join(ContentDigest::of(original).hex()),
            corrupt,
        )
        .unwrap();
        let error = fixture
            .import("corrupt", GuestPlatform::LinuxAmd64)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("descriptor hash/size mismatch"),
            "{object}: {error}"
        );
    }
}

#[test]
fn incorrect_diff_id_is_rejected_before_any_layer_entries_are_applied() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[TarEntry::file("must-not-extract", b"original")]);
    let image = fixture.image("amd64", &[(&raw, ZSTD)], true);
    fixture.index(&[image.descriptor]);
    let error = fixture
        .import("bad-diff", GuestPlatform::LinuxAmd64)
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("uncompressed layer hash mismatch"),
        "{error}"
    );
    assert_eq!(
        fs::read_dir(fixture.temp.path().join("bad-diff/files"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn oversized_oci_config_is_rejected_before_filesystem_preparation() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[TarEntry::file("app", b"must not unpack")]);
    let image = fixture.image("amd64", &[(&raw, RAW)], false);
    let mut config: Value = serde_json::from_slice(&image.config).unwrap();
    config["padding"] = json!("x".repeat(8 * 1024 * 1024 + 1));
    let config = serde_json::to_vec(&config).unwrap();
    fixture.blob(&config);
    let mut manifest: Value = serde_json::from_slice(&image.manifest).unwrap();
    manifest["config"] = descriptor(&config, CONFIG);
    let manifest = serde_json::to_vec(&manifest).unwrap();
    fixture.blob(&manifest);
    let mut entry = descriptor(&manifest, MANIFEST);
    entry["platform"] = json!({"os":"linux","architecture":"amd64"});
    fixture.index(&[entry]);
    let error = fixture
        .import("oversized-config", GuestPlatform::LinuxAmd64)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("metadata") && error.to_string().contains("limit"),
        "{error}"
    );
    assert!(!fixture.temp.path().join("oversized-config/files").exists());
}

#[test]
fn oversized_docker_save_config_and_manifest_are_rejected_before_unpack() {
    for oversized in ["config", "manifest"] {
        let fixture = Fixture::new();
        let raw = tar_bytes(&[TarEntry::file("app", b"must not unpack")]);
        let image = fixture.image("amd64", &[(&raw, RAW)], false);
        let mut config: Value = serde_json::from_slice(&image.config).unwrap();
        let mut manifest = json!([{
            "Config":"config.json","RepoTags":["fixture:latest"],"Layers":["layer.tar"]
        }]);
        let padding = json!("x".repeat(8 * 1024 * 1024 + 1));
        if oversized == "config" {
            config["padding"] = padding;
        } else {
            manifest[0]["Padding"] = padding;
        }
        let config = serde_json::to_vec(&config).unwrap();
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let archive = fixture.temp.path().join("oversized.tar");
        fs::write(
            &archive,
            tar_bytes(&[
                TarEntry::file("manifest.json", &manifest),
                TarEntry::file("config.json", &config),
                TarEntry::file("layer.tar", &raw),
            ]),
        )
        .unwrap();
        let destination = fixture.temp.path().join("rejected");
        let error = NativeOci::default()
            .import(
                ImageInput::DockerArchive {
                    path: &archive,
                    reference: None,
                },
                &service(),
                &destination,
                &fixture.store,
                GuestPlatform::LinuxAmd64,
                None,
            )
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("metadata") && error.to_string().contains("limit"),
            "{oversized}: {error}"
        );
        assert!(!destination.join("files").exists());
    }
}

#[test]
fn oci_and_docker_archives_produce_equivalent_retained_images_and_guest_files() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[TarEntry::file("app", b"archive bytes")]);
    let image = fixture.image("amd64", &[(&raw, RAW)], false);
    fixture.index(&[image.descriptor]);
    let expected = fixture
        .import("layout-import", GuestPlatform::LinuxAmd64)
        .unwrap();
    let expected_tree = fixture.package(&expected.filesystem, "expected.cpio");
    let mut builder = tar::Builder::new(Vec::new());
    for name in ["oci-layout", "index.json"] {
        let data = fs::read(fixture.layout.join(name)).unwrap();
        let entry = TarEntry::file(name, &data);
        builder.append(&header(&entry), entry.data).unwrap();
    }
    for bytes in [&image.manifest, &image.config, &raw] {
        let name = format!("blobs/sha256/{}", ContentDigest::of(bytes).hex());
        let entry = TarEntry::file(&name, bytes);
        builder.append(&header(&entry), entry.data).unwrap();
    }
    let oci = builder.into_inner().unwrap();
    for (name, bytes) in [
        ("plain", oci.clone()),
        ("gzip", gzip(&oci)),
        ("zstd", zstd::stream::encode_all(oci.as_slice(), 1).unwrap()),
    ] {
        let path = fixture.temp.path().join(format!("{name}.tar"));
        fs::write(&path, bytes).unwrap();
        let imported = NativeOci::default()
            .import(
                ImageInput::OciArchive {
                    path: &path,
                    reference: None,
                },
                &service(),
                &fixture.temp.path().join(name),
                &fixture.store,
                GuestPlatform::LinuxAmd64,
                None,
            )
            .unwrap();
        assert_eq!(imported.image, expected.image);
        assert_eq!(
            fixture.package(&imported.filesystem, &format!("{name}.cpio")),
            expected_tree
        );
    }
    let saved_manifest = serde_json::to_vec(&json!([{"Config":"config.json","RepoTags":["fixture:latest"],"Layers":["layer/layer.tar"]}])).unwrap();
    let docker = tar_bytes(&[
        TarEntry::file("manifest.json", &saved_manifest),
        TarEntry::file("config.json", &image.config),
        TarEntry::file("layer/layer.tar", &raw),
    ]);
    let path = fixture.temp.path().join("docker.tar");
    fs::write(&path, docker).unwrap();
    let mut previous_manifest = None;
    for name in ["docker-one", "docker-two"] {
        let imported = NativeOci::default()
            .import(
                ImageInput::DockerArchive {
                    path: &path,
                    reference: Some("fixture:latest"),
                },
                &service(),
                &fixture.temp.path().join(name),
                &fixture.store,
                GuestPlatform::LinuxAmd64,
                None,
            )
            .unwrap();
        assert_eq!(imported.image.config, expected.image.config);
        assert_eq!(imported.image.layers, expected.image.layers);
        assert_eq!(imported.image.launch, expected.image.launch);
        assert_eq!(
            fixture.package(&imported.filesystem, &format!("{name}.cpio")),
            expected_tree
        );
        assert_eq!(
            fixture
                .store
                .read(&imported.auxiliary["archive-manifest"])
                .unwrap(),
            saved_manifest
        );
        if let Some(previous) = previous_manifest {
            assert_eq!(imported.image.manifest, previous);
        }
        previous_manifest = Some(imported.image.manifest);
    }
}

#[test]
fn local_daemon_export_accepts_short_names_and_image_ids_without_matching_saved_tags() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[TarEntry::file("app", b"finished local image")]);
    let image = fixture.image("amd64", &[(&raw, RAW)], false);
    let saved_manifest = serde_json::to_vec(&json!([{
        "Config":"config.json","RepoTags":["fixture:latest"],"Layers":["layer.tar"]
    }]))
    .unwrap();
    let archive = fixture.temp.path().join("saved.tar");
    fs::write(
        &archive,
        tar_bytes(&[
            TarEntry::file("manifest.json", &saved_manifest),
            TarEntry::file("config.json", &image.config),
            TarEntry::file("layer.tar", &raw),
        ]),
    )
    .unwrap();
    let client = fixture.temp.path().join("export-client");
    let request = fixture.temp.path().join("requested-reference");
    let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    fs::write(&client,format!(
        "#!/bin/sh\nset -eu\n[ \"$1\" = image ]\n[ \"$2\" = save ]\n[ \"$3\" = --output ]\nprintf '%s' \"$5\" > {}\nexec cp {} \"$4\"\n",quote(&request),quote(&archive)
    )).unwrap();
    fs::set_permissions(&client, fs::Permissions::from_mode(0o755)).unwrap();
    for (name, reference, podman) in [
        ("docker-short", "fixture", false),
        ("docker-id", "sha256:1234567890", false),
        ("podman-short", "fixture", true),
        ("podman-id", "sha256:1234567890", true),
    ] {
        let input = if podman {
            ImageInput::Podman {
                client: &client,
                reference,
            }
        } else {
            ImageInput::Docker {
                client: &client,
                reference,
            }
        };
        let imported = NativeOci::default()
            .import(
                input,
                &service(),
                &fixture.temp.path().join(name),
                &fixture.store,
                GuestPlatform::LinuxAmd64,
                None,
            )
            .unwrap();
        assert_eq!(fs::read_to_string(&request).unwrap(), reference);
        assert_eq!(imported.provenance["local-image-reference"], reference);
        assert_eq!(
            fixture.store.read(&imported.image.config).unwrap(),
            image.config
        );
        assert_eq!(
            fixture.package(&imported.filesystem, &format!("{name}.cpio"))["rootfs/app"].data,
            b"finished local image"
        );
    }
}

#[test]
fn invalid_local_references_are_rejected_before_either_export_client_runs() {
    let fixture = Fixture::new();
    let marker = fixture.temp.path().join("exporter-invoked");
    let outside = fixture.temp.path().join("outside");
    let client = fixture.temp.path().join("export-client");
    let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    fs::write(
        &client,
        format!(
            "#!/bin/sh\nprintf invoked > {}\nprintf unwanted > {}\n",
            quote(&marker),
            quote(&outside)
        ),
    )
    .unwrap();
    fs::set_permissions(&client, fs::Permissions::from_mode(0o755)).unwrap();
    for podman in [false, true] {
        for (index, reference) in [
            "",
            "--output=outside",
            "image name",
            "image\nname",
            "image\tname",
            "image\u{1}name",
        ]
        .into_iter()
        .enumerate()
        {
            let input = if podman {
                ImageInput::Podman {
                    client: &client,
                    reference,
                }
            } else {
                ImageInput::Docker {
                    client: &client,
                    reference,
                }
            };
            let directory = fixture.temp.path().join(format!(
                "rejected-{}-{index}",
                if podman { "podman" } else { "docker" }
            ));
            let error = NativeOci::default()
                .import(
                    input,
                    &service(),
                    &directory,
                    &fixture.store,
                    GuestPlatform::LinuxAmd64,
                    None,
                )
                .err()
                .unwrap();
            assert!(matches!(error, Error::Setup(_)), "{reference:?}: {error}");
            assert_eq!(error.to_string(),"local image reference: expected an existing image name or ID without command options");
            assert!(!marker.exists(), "export client invoked for {reference:?}");
            assert!(
                !outside.exists(),
                "export client wrote outside its import directory for {reference:?}"
            );
        }
    }
}

#[test]
fn unsafe_and_duplicate_outer_archive_objects_are_rejected() {
    for entries in [
        vec![TarEntry::file("../outside", b"bad")],
        vec![
            TarEntry::file("manifest.json", b"[]"),
            TarEntry::file("./manifest.json", b"[]"),
        ],
        vec![TarEntry::link("manifest.json", "/outside", false)],
    ] {
        let fixture = Fixture::new();
        let path = fixture.temp.path().join("hostile.tar");
        fs::write(&path, tar_bytes(&entries)).unwrap();
        let error = NativeOci::default()
            .import(
                ImageInput::DockerArchive {
                    path: &path,
                    reference: None,
                },
                &service(),
                &fixture.temp.path().join("rejected"),
                &fixture.store,
                GuestPlatform::LinuxAmd64,
                None,
            )
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("unsafe archive path")
                || error.to_string().contains("duplicate or non-regular"),
            "{error}"
        );
        assert!(!fixture.temp.path().join("outside").exists());
    }
}

#[test]
fn guest_archive_preserves_linux_metadata_and_forward_hardlinks_without_host_paths() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[
        TarEntry::link("a-forward", "z-file", true),
        TarEntry::link("absolute", "/z-file", false),
        TarEntry::file("z-file", b"shared inode data"),
    ]);
    let image = fixture.image("amd64", &[(&raw, RAW)], false);
    fixture.index(&[image.descriptor]);
    let imported = fixture.import("import", GuestPlatform::LinuxAmd64).unwrap();
    let entries = fixture.package(&imported.filesystem, "metadata.cpio");
    let first = &entries["rootfs/a-forward"];
    let last = &entries["rootfs/z-file"];
    assert_eq!(
        (first.mode, first.uid, first.gid, first.mtime),
        (0o100640, 123, 456, 123456789)
    );
    assert_eq!(first.inode, last.inode);
    assert_eq!((first.links, last.links), (2, 2));
    assert_eq!(first.data, b"shared inode data");
    assert!(last.data.is_empty());
    assert_eq!(entries["rootfs/absolute"].mode & 0o170000, 0o120000);
    assert_eq!(entries["rootfs/absolute"].data, b"/z-file");
    let spool = fixture.temp.path().join("import/files");
    for entry in fs::read_dir(spool).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_name().to_str().unwrap().parse::<u32>().is_ok());
        assert!(entry.file_type().unwrap().is_file());
    }
    assert!(!fixture.temp.path().join("import/z-file").exists());
}

#[test]
fn overwrites_whiteouts_and_late_opaque_markers_affect_only_lower_entries() {
    let fixture = Fixture::new();
    let lower = tar_bytes(&[
        TarEntry::file("keep", b"old keep"),
        TarEntry::link("old-alias", "keep", true),
        TarEntry::file("gone/child", b"gone"),
        TarEntry::file("opaque/old", b"old"),
    ]);
    let upper = tar_bytes(&[
        TarEntry::file("keep", b"new keep"),
        TarEntry::file("opaque/new", b"new"),
        TarEntry::file("opaque/.wh..wh..opq", b""),
        TarEntry::file(".wh.gone", b""),
    ]);
    let image = fixture.image("amd64", &[(&lower, RAW), (&upper, GZIP)], false);
    fixture.index(&[image.descriptor]);
    let imported = fixture.import("import", GuestPlatform::LinuxAmd64).unwrap();
    let entries = fixture.package(&imported.filesystem, "whiteouts.cpio");
    assert_eq!(entries["rootfs/keep"].data, b"new keep");
    assert_eq!(entries["rootfs/old-alias"].data, b"old keep");
    assert_ne!(
        entries["rootfs/old-alias"].inode,
        entries["rootfs/keep"].inode
    );
    assert_eq!(entries["rootfs/opaque/new"].data, b"new");
    assert!(!entries.contains_key("rootfs/opaque/old"));
    assert!(!entries
        .keys()
        .any(|name| name.starts_with("rootfs/gone") || name.contains(".wh.")));
}

#[test]
fn directory_metadata_and_fractional_pax_mtime_survive_packaging() {
    let fixture = Fixture::new();
    let mut builder = tar::Builder::new(Vec::new());
    let directory = TarEntry {
        kind: tar::EntryType::Directory,
        mode: 0o2750,
        uid: 1000,
        gid: 2000,
        mtime: 900,
        ..TarEntry::file("owned", b"")
    };
    builder.append(&header(&directory), directory.data).unwrap();
    builder
        .append_pax_extensions([
            ("mtime", b"987654321.125".as_slice()),
            ("uid", b"4000000000".as_slice()),
            ("gid", b"4294967295".as_slice()),
        ])
        .unwrap();
    let file = TarEntry::file("owned/file", b"timestamp");
    builder.append(&header(&file), file.data).unwrap();
    let raw = builder.into_inner().unwrap();
    let image = fixture.image("amd64", &[(&raw, RAW)], false);
    fixture.index(&[image.descriptor]);
    let imported = fixture.import("import", GuestPlatform::LinuxAmd64).unwrap();
    let entries = fixture.package(&imported.filesystem, "pax.cpio");
    let directory = &entries["rootfs/owned"];
    assert_eq!(
        (
            directory.mode,
            directory.uid,
            directory.gid,
            directory.mtime
        ),
        (0o042750, 1000, 2000, 900)
    );
    assert_eq!(entries["rootfs/owned/file"].mtime, 987654321);
    assert_eq!(entries["rootfs/owned/file"].uid, 4_000_000_000);
    assert_eq!(entries["rootfs/owned/file"].gid, u32::MAX);
}

#[test]
fn malformed_or_overflowing_pax_ownership_and_timestamps_fail_before_file_copy() {
    for key in ["uid", "gid", "mtime"] {
        let values: &[&[u8]] = if key == "mtime" {
            &[
                b"",
                b"1.",
                b".1",
                b"1.bad",
                b"1.2.3",
                b"-1.5",
                b"+1.2",
                b"1. 2",
                b"4294967296.1",
                b"18446744073709551616",
                b"1.2\0",
                b"1.\xff",
            ]
        } else {
            &[
                b"",
                b"owner",
                b"-1",
                b"1.5",
                b"+1",
                b"1 ",
                b"4294967296",
                b"18446744073709551616",
                b"1\0",
                b"\xff",
            ]
        };
        for value in values {
            let fixture = Fixture::new();
            let mut builder = tar::Builder::new(Vec::new());
            builder.append_pax_extensions([(key, *value)]).unwrap();
            let entry = TarEntry::file("must-not-copy", b"data");
            builder.append(&header(&entry), entry.data).unwrap();
            let raw = builder.into_inner().unwrap();
            let image = fixture.image("amd64", &[(&raw, RAW)], false);
            fixture.index(&[image.descriptor]);
            let error = fixture
                .import("rejected", GuestPlatform::LinuxAmd64)
                .err()
                .unwrap();
            assert!(error.to_string().contains(key), "{key}={value:?}: {error}");
            assert_eq!(
                fs::read_dir(fixture.temp.path().join("rejected/files"))
                    .unwrap()
                    .count(),
                0,
                "copied file for {key}={value:?}"
            );
        }
    }
}

#[test]
fn duplicate_pax_keys_are_rejected_even_when_the_values_agree() {
    for key in ["uid", "gid", "mtime", "uname"] {
        let fixture = Fixture::new();
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_pax_extensions([(key, b"1".as_slice()), (key, b"1".as_slice())])
            .unwrap();
        let entry = TarEntry::file("must-not-copy", b"data");
        builder.append(&header(&entry), entry.data).unwrap();
        let raw = builder.into_inner().unwrap();
        let image = fixture.image("amd64", &[(&raw, RAW)], false);
        fixture.index(&[image.descriptor]);
        let error = fixture
            .import("rejected", GuestPlatform::LinuxAmd64)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains(&format!("duplicate image metadata {key}")),
            "{error}"
        );
    }
}

#[test]
fn pax_timestamp_whole_seconds_and_numeric_fractions_preserve_newc_boundaries() {
    for (value, expected) in [
        ("0", 0),
        ("0.000000001", 0),
        ("42.999", 42),
        ("4294967295.999", u32::MAX),
    ] {
        let fixture = Fixture::new();
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_pax_extensions([("mtime", value.as_bytes())])
            .unwrap();
        let entry = TarEntry::file("time", b"data");
        builder.append(&header(&entry), entry.data).unwrap();
        let raw = builder.into_inner().unwrap();
        let image = fixture.image("amd64", &[(&raw, RAW)], false);
        fixture.index(&[image.descriptor]);
        let imported = fixture.import("import", GuestPlatform::LinuxAmd64).unwrap();
        assert_eq!(
            fixture.package(&imported.filesystem, "time.cpio")["rootfs/time"].mtime,
            expected
        );
    }
}

#[test]
fn symlinked_parent_and_whiteout_resolve_inside_guest_namespace() {
    let fixture = Fixture::new();
    let lower = tar_bytes(&[
        TarEntry::file("real/old", b"lower"),
        TarEntry::link("alias", "/real", false),
        TarEntry::link("escape", "../../real", false),
    ]);
    let upper = tar_bytes(&[
        TarEntry::file("alias/new", b"inside absolute target"),
        TarEntry::file("escape/other", b"inside clamped relative target"),
        TarEntry::file("alias/.wh.old", b""),
    ]);
    let image = fixture.image("amd64", &[(&lower, RAW), (&upper, RAW)], false);
    fixture.index(&[image.descriptor]);
    let imported = fixture.import("import", GuestPlatform::LinuxAmd64).unwrap();
    let entries = fixture.package(&imported.filesystem, "symlink-paths.cpio");
    assert_eq!(entries["rootfs/real/new"].data, b"inside absolute target");
    assert_eq!(
        entries["rootfs/real/other"].data,
        b"inside clamped relative target"
    );
    assert!(!entries.contains_key("rootfs/real/old"));
    assert!(!entries.contains_key("rootfs/alias/new"));
    assert!(!fixture.temp.path().join("real").exists());
}

#[test]
fn named_whiteout_keeps_a_replacement_created_in_the_same_layer() {
    let fixture = Fixture::new();
    let lower = tar_bytes(&[TarEntry::file("replaced", b"lower")]);
    let upper = tar_bytes(&[
        TarEntry::file("replaced", b"upper"),
        TarEntry::file(".wh.replaced", b""),
    ]);
    let image = fixture.image("amd64", &[(&lower, RAW), (&upper, RAW)], false);
    fixture.index(&[image.descriptor]);
    let imported = fixture.import("import", GuestPlatform::LinuxAmd64).unwrap();
    assert_eq!(
        fixture.package(&imported.filesystem, "replacement.cpio")["rootfs/replaced"].data,
        b"upper"
    );
}

#[test]
fn unresolved_cyclic_and_nonregular_hardlinks_are_rejected() {
    for entries in [
        vec![TarEntry::link("missing", "absent", true)],
        vec![
            TarEntry::link("a", "b", true),
            TarEntry::link("b", "a", true),
        ],
        vec![
            TarEntry::link("target", "/absolute", false),
            TarEntry::link("hard", "target", true),
        ],
    ] {
        let fixture = Fixture::new();
        let raw = tar_bytes(&entries);
        let image = fixture.image("amd64", &[(&raw, RAW)], false);
        fixture.index(&[image.descriptor]);
        let error = fixture
            .import("rejected", GuestPlatform::LinuxAmd64)
            .err()
            .unwrap();
        assert!(error.to_string().contains("hardlink"), "{error}");
    }
}

#[test]
fn hostile_paths_duplicates_xattrs_and_special_files_are_rejected_without_extraction() {
    let mut pax = tar::Builder::new(Vec::new());
    pax.append_pax_extensions([("SCHILY.xattr.security.capability", b"capability".as_slice())])
        .unwrap();
    let data = TarEntry::file("capabilities", b"data");
    pax.append(&header(&data), data.data).unwrap();
    let fixtures = [
        (
            "parent",
            tar_bytes(&[TarEntry::file("../outside", b"bad")]),
            "unsafe archive path",
        ),
        (
            "absolute",
            tar_bytes(&[TarEntry::file("/outside", b"bad")]),
            "unsafe archive path",
        ),
        (
            "duplicate",
            tar_bytes(&[
                TarEntry::file("same", b"first"),
                TarEntry::file("./same", b"second"),
            ]),
            "duplicate layer path",
        ),
        (
            "xattr",
            pax.into_inner().unwrap(),
            "unsupported image metadata",
        ),
        (
            "fifo",
            tar_bytes(&[TarEntry {
                kind: tar::EntryType::Fifo,
                ..TarEntry::file("fifo", b"")
            }]),
            "unsupported special",
        ),
    ];
    for (name, raw, expected) in fixtures {
        let fixture = Fixture::new();
        let image = fixture.image("amd64", &[(&raw, RAW)], false);
        fixture.index(&[image.descriptor]);
        let error = fixture
            .import(name, GuestPlatform::LinuxAmd64)
            .err()
            .unwrap();
        assert!(error.to_string().contains(expected), "{name}: {error}");
        assert!(!fixture.temp.path().join("outside").exists());
        assert!(!fixture.temp.path().join(name).join("same").exists());
    }
}

#[test]
fn forward_hardlink_cannot_leave_an_entry_under_a_replaced_symlink_parent() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[
        TarEntry::link("a/init", "z", true),
        TarEntry::link("a", "/", false),
        TarEntry::file("z", b"data"),
    ]);
    let image = fixture.image("amd64", &[(&raw, RAW)], false);
    fixture.index(&[image.descriptor]);
    let error = fixture
        .import("invalid-parent", GuestPlatform::LinuxAmd64)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("parent") && error.to_string().contains("hardlink"),
        "{error}"
    );
}

#[test]
fn oversized_tar_metadata_is_rejected_from_header_before_reading_missing_payload() {
    for kind in [
        tar::EntryType::XHeader,
        tar::EntryType::GNULongName,
        tar::EntryType::GNULongLink,
    ] {
        let fixture = Fixture::new();
        let entry = TarEntry {
            kind,
            ..TarEntry::file("metadata", b"")
        };
        let mut header = header(&entry);
        header.set_size(2 * 1024 * 1024);
        header.set_cksum();
        let raw = header.as_bytes().to_vec();
        let image = fixture.image("amd64", &[(&raw, RAW)], false);
        fixture.index(&[image.descriptor]);
        let error = fixture
            .import("too-large", GuestPlatform::LinuxAmd64)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("metadata exceeds 1 MiB"),
            "{error}"
        );
    }
}

#[test]
fn cancellation_prevents_import_and_interrupts_file_copy_and_packaging() {
    let fixture = Fixture::new();
    let token = CancellationToken::default();
    let context =
        PreparationContext::new(fixture.temp.path().join("evidence"), token.clone()).unwrap();
    token.cancel();
    let destination = fixture.temp.path().join("never-created");
    let error = NativeOci::default()
        .import(
            ImageInput::OciLayout {
                path: &fixture.layout,
                reference: None,
            },
            &service(),
            &destination,
            &fixture.store,
            GuestPlatform::LinuxAmd64,
            Some(&context),
        )
        .err()
        .unwrap();
    assert!(matches!(error, Error::PreparationCancelled));
    assert!(!destination.exists());

    let data = vec![b'x'; 1024 * 1024];
    let raw = tar_bytes(&[TarEntry::file("large", &data)]);
    let mut tree = ImageFilesystem::new(&fixture.temp.path().join("cancelled-spools")).unwrap();
    let calls = Cell::new(0);
    let check = || {
        calls.set(calls.get() + 1);
        if calls.get() >= 6 {
            Err(Error::PreparationCancelled)
        } else {
            Ok(())
        }
    };
    assert!(matches!(
        tree.apply_layer(raw.as_slice(), &check),
        Err(Error::PreparationCancelled)
    ));
    assert!(calls.get() >= 6);
    let mut complete = ImageFilesystem::new(&fixture.temp.path().join("complete-spools")).unwrap();
    complete.apply_layer(raw.as_slice(), &|| Ok(())).unwrap();
    let helper = fixture.temp.path().join("helper");
    fs::write(&helper, b"helper").unwrap();
    calls.set(0);
    let output = fixture.temp.path().join("cancelled.cpio");
    assert!(matches!(
        build_initramfs_image(&complete, &helper, &helper, None, b"recipe", &output, &check),
        Err(Error::PreparationCancelled)
    ));
    assert!(fs::metadata(output).unwrap().len() < data.len() as u64);
}

#[test]
fn loopback_registry_authenticates_and_fetches_index_children_via_manifest_routes() {
    let fixture = Fixture::new();
    let raw = tar_bytes(&[TarEntry::file("app", b"registry data")]);
    let image = fixture.image("amd64", &[(&raw, GZIP)], false);
    let index = fixture.index(&[image.descriptor]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let realm = format!("{endpoint}token");
    let manifest_route = format!("/v2/library/fixture/manifests/{}", digest(&image.manifest));
    let config_route = format!("/v2/library/fixture/blobs/{}", digest(&image.config));
    let layer_route = format!("/v2/library/fixture/blobs/{}", digest(&image.layers[0]));
    let objects = BTreeMap::from([
        ("/v2/library/fixture/manifests/latest".to_owned(), index),
        (manifest_route.clone(), image.manifest.clone()),
        (config_route.clone(), image.config.clone()),
        (layer_route.clone(), image.layers[0].clone()),
    ]);
    let (send, receive) = mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut routes = Vec::new();
        while routes.len() < 6 && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("mock registry accept: {error}"),
            };
            // Accepted sockets inherit nonblocking mode on macOS.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 16384);
            }
            let request = String::from_utf8(request).unwrap();
            let route = request
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            routes.push(route.clone());
            let (status, extra, body) = if route.starts_with("/token?") {
                assert!(route.contains("scope=repository%3Alibrary%2Ffixture%3Apull"));
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                (
                    "200 OK",
                    String::new(),
                    br#"{"token":"fixture-token"}"#.to_vec(),
                )
            } else if !request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-token")
            {
                (
                    "401 Unauthorized",
                    format!("WWW-Authenticate: Bearer realm=\"{realm}\",service=\"fixture\"\r\n"),
                    Vec::new(),
                )
            } else {
                let body = objects
                    .get(&route)
                    .unwrap_or_else(|| panic!("unexpected registry route: {route}"))
                    .clone();
                (
                    "200 OK",
                    format!("Docker-Content-Digest: {}\r\n", digest(&body)),
                    body,
                )
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
        send.send(routes).unwrap();
    });
    let native = NativeOci {
        registry: RegistryClient::with_endpoint(&endpoint).unwrap(),
    };
    let result = native.import(
        ImageInput::Registry("fixture"),
        &service(),
        &fixture.temp.path().join("registry-import"),
        &fixture.store,
        GuestPlatform::LinuxAmd64,
        None,
    );
    let routes = receive.recv_timeout(Duration::from_secs(20)).unwrap();
    server.join().unwrap();
    let imported = result.unwrap();
    assert_eq!(routes.len(), 6);
    assert!(routes.contains(&manifest_route));
    assert!(routes.contains(&config_route));
    assert!(routes.contains(&layer_route));
    assert_eq!(
        fixture.store.read(&imported.image.manifest).unwrap(),
        image.manifest
    );
    assert_eq!(
        fixture.package(&imported.filesystem, "registry.cpio")["rootfs/app"].data,
        b"registry data"
    );
}

/// Explicit external-network smoke test. Supply an anonymous public registry
/// reference pinned with @sha256:, then run this test with --ignored --exact.
/// SEEDBOX_PUBLIC_IMAGE_PLATFORM accepts linux/arm64 (default) or linux/amd64.
/// This probe reads only those two test options and uses no managed credentials.
#[test]
#[ignore = "requires an explicitly supplied pinned public image and external registry access"]
fn public_registry_pinned_image_smoke() {
    let reference = std::env::var("SEEDBOX_PUBLIC_IMAGE")
        .expect("set SEEDBOX_PUBLIC_IMAGE to an anonymous public image pinned with @sha256:");
    let parsed = RegistryReference::parse(&reference).expect("valid public registry reference");
    assert!(
        reference.contains("@sha256:") && parsed.selector.starts_with("sha256:"),
        "public smoke image must be digest pinned"
    );
    let platform = match std::env::var("SEEDBOX_PUBLIC_IMAGE_PLATFORM").as_deref() {
        Ok("linux/amd64") => GuestPlatform::LinuxAmd64,
        Ok("linux/arm64") | Err(std::env::VarError::NotPresent) => GuestPlatform::LinuxArm64,
        _ => panic!("SEEDBOX_PUBLIC_IMAGE_PLATFORM must be linux/arm64 or linux/amd64"),
    };
    let fixture = Fixture::new();
    let imported = NativeOci::default()
        .import(
            ImageInput::Registry(&reference),
            &service(),
            &fixture.temp.path().join("public-image"),
            &fixture.store,
            platform,
            None,
        )
        .expect("anonymous pinned registry import");
    let manifest_bytes = fixture.store.read(&imported.image.manifest).unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let config_bytes = fixture.store.read(&imported.image.config).unwrap();
    let config: Value = serde_json::from_slice(&config_bytes).unwrap();
    assert_eq!(config["os"], "linux");
    assert_eq!(config["architecture"], platform.architecture());
    assert_eq!(manifest["config"]["digest"], digest(&config_bytes));
    assert_eq!(
        manifest["layers"].as_array().unwrap().len(),
        imported.image.layers.len()
    );
    for (descriptor, retained) in manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&imported.image.layers)
    {
        fixture.store.verify(retained).unwrap();
        assert_eq!(descriptor["digest"], format!("sha256:{}", retained.hex()));
        assert_eq!(
            descriptor["size"].as_u64().unwrap(),
            fixture.store.size(retained).unwrap()
        );
    }
    let selected_document = imported
        .auxiliary
        .get("source-index")
        .unwrap_or(&imported.image.manifest);
    assert_eq!(
        format!("sha256:{}", selected_document.hex()),
        parsed.selector
    );
    for retained in imported.auxiliary.values() {
        fixture.store.verify(retained).unwrap();
    }
    let packaged = fixture.package(&imported.filesystem, "public-image.cpio");
    assert!(packaged.contains_key("rootfs"));
    eprintln!("public image: {}\nplatform: {}\nresolved manifest: sha256:{}\nretained layers: {}\npackaged image entries: {}",reference,platform.as_str(),imported.image.manifest.hex(),imported.image.layers.len(),packaged.keys().filter(|name| name.starts_with("rootfs/")).count());
}
