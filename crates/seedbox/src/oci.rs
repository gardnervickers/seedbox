//! Registry images and Dockerfile builds converge through a verified OCI import.
//!
//! Skopeo preserves the original platform manifest in its dir transport. A
//! separately normalized OCI layout is unpacked by umoci, whose layer handling
//! includes whiteouts, opaque directories, links, permissions and ownership.

use crate::{
    config::{ImageSource, Service},
    execution::PreparedImage,
    image::{resolve_launch, ImageDefaults},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciTools {
    pub skopeo: PathBuf,
    pub umoci: PathBuf,
    pub docker: PathBuf,
    /// Explicit timestamp policy for new layers from the admitted native Podman.
    pub build_epoch: Option<u32>,
}

impl Default for OciTools {
    fn default() -> Self {
        Self {
            skopeo: "skopeo".into(),
            umoci: "umoci".into(),
            docker: "podman".into(),
            build_epoch: None,
        }
    }
}

/// Linux filesystems stay on Linux; a container transport returns an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootFilesystem {
    Directory(PathBuf),
    Archive(PathBuf),
}

pub trait ImageTransport {
    fn import(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context: Option<(&crate::preparation::PreparationContext, &str)>,
        frozen: Option<&FrozenContext>,
        architecture: crate::guest::Architecture,
    ) -> Result<ImportedImage>;
}
impl ImageTransport for OciTools {
    fn import(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context: Option<(&crate::preparation::PreparationContext, &str)>,
        frozen: Option<&FrozenContext>,
        architecture: crate::guest::Architecture,
    ) -> Result<ImportedImage> {
        self.import_frozen_for_arch(service, directory, store, context, frozen, architecture)
    }
}

#[derive(Clone)]
pub struct ImportedImage {
    pub image: PreparedImage,
    pub rootfs: RootFilesystem,
    pub auxiliary: BTreeMap<String, ContentDigest>,
    pub provenance: BTreeMap<String, String>,
}

impl ImportedImage {
    /// Reuse verified image bytes, resolving this service's launch independently.
    pub(crate) fn for_service(&self, service: &Service, store: &ContentStore) -> Result<Self> {
        let config: ImageConfig = serde_json::from_slice(&store.read(&self.image.config)?)?;
        let defaults: ImageDefaults = serde_json::from_value(config.config)?;
        let mut imported = self.clone();
        imported.image.launch = resolve_launch(service, &defaults)?;
        Ok(imported)
    }
}

/// One retained context, frozen before any service-specific build or launch.
pub struct FrozenContext {
    pub root: PathBuf,
    pub digest: ContentDigest,
}

impl FrozenContext {
    pub fn capture(context: &Path, directory: &Path, store: &ContentStore) -> Result<Self> {
        fs::create_dir_all(directory)?;
        let snapshot = directory.join("context.tar");
        snapshot_context(context, &snapshot)?;
        let digest = store.put_file(&snapshot)?;
        let root = directory.join("build-context");
        fs::create_dir(&root)?;
        tar::Archive::new(fs::File::open(&snapshot)?).unpack(&root)?;
        Ok(Self { root, digest })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ImageConfig {
    os: String,
    architecture: String,
    config: serde_json::Value,
}

impl OciTools {
    /// `directory` must be a fresh per-image directory; the caller owns its
    /// lifetime until guest packaging finishes. No simulation or MSR work occurs.
    pub fn import(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
    ) -> Result<ImportedImage> {
        self.import_with_context(service, directory, store, None)
    }
    pub(crate) fn import_with_context(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context_options: Option<(&crate::preparation::PreparationContext, &str)>,
    ) -> Result<ImportedImage> {
        self.import_frozen(service, directory, store, context_options, None)
    }
    pub(crate) fn import_frozen(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context_options: Option<(&crate::preparation::PreparationContext, &str)>,
        frozen: Option<&FrozenContext>,
    ) -> Result<ImportedImage> {
        self.import_frozen_for_arch(
            service,
            directory,
            store,
            context_options,
            frozen,
            crate::guest::Architecture::Amd64,
        )
    }
    pub(crate) fn import_frozen_for_arch(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context_options: Option<(&crate::preparation::PreparationContext, &str)>,
        frozen: Option<&FrozenContext>,
        architecture: crate::guest::Architecture,
    ) -> Result<ImportedImage> {
        let local_builder =
            context_options.is_some() || self.build_epoch.is_some() || frozen.is_some();
        fs::create_dir_all(directory)?;
        let run = |command: &mut Command,
                   phase: &str,
                   tag: Option<&str>,
                   build: bool,
                   cleanup: bool|
         -> Result<()> {
            if let Some((evidence, name)) = context_options {
                evidence.run(command, &format!("{name}-{phase}"), tag, build, cleanup)
            } else {
                checked(command, &directory.join(format!("{phase}.log")))
            }
        };
        let tool_version = |path: &Path, phase: &str| -> Result<String> {
            if let Some((evidence, name)) = context_options {
                evidence.version(path, &format!("{name}-{phase}"))
            } else {
                version(path)
            }
        };
        let raw = directory.join("source");
        let layout = directory.join("oci");
        let bundle = directory.join("bundle");
        if raw.exists() || layout.exists() || bundle.exists() {
            return Err(Error::Execution(
                "image import requires fresh output paths".into(),
            ));
        }
        let mut auxiliary = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        provenance.insert(
            "skopeo".into(),
            tool_version(&self.skopeo, "skopeo-version")?,
        );
        provenance.insert("umoci".into(), tool_version(&self.umoci, "umoci-version")?);
        let source = match &service.source {
            ImageSource::Registry { reference } => format!("docker://{reference}"),
            ImageSource::Build {
                context,
                dockerfile,
            } => {
                // Build precisely the retained context snapshot, not a live tree
                // which could change between fingerprinting and Docker reading it.
                let relative = dockerfile
                    .strip_prefix(context)
                    .map_err(|_| Error::Validation {
                        path: "build.dockerfile".into(),
                        message: "Dockerfile must be inside the build context".into(),
                    })?;
                let owned;
                let frozen = match frozen {
                    Some(frozen) => frozen,
                    None => {
                        owned = FrozenContext::capture(context, directory, store)?;
                        &owned
                    }
                };
                auxiliary.insert("build-context".into(), frozen.digest.clone());
                let frozen_context = &frozen.root;
                auxiliary.insert(
                    "dockerfile".into(),
                    store.put_file(&frozen_context.join(relative))?,
                );
                provenance.insert(
                    "docker".into(),
                    tool_version(&self.docker, "builder-version")?,
                );
                if let Some(epoch) = self.build_epoch {
                    provenance.insert("build-epoch".into(), epoch.to_string());
                    provenance.insert(
                        "build-timestamp-policy".into(),
                        "podman --source-date-epoch --rewrite-timestamp; new-layer mtimes clamped"
                            .into(),
                    );
                }
                let archive = directory.join("build.tar");
                // A unique tag avoids deleting any pre-existing user image. Only
                // this private tag is removed; --no-prune keeps its untagged
                // parent layers, which are the build cache for the next run.
                let tag_directory = tempfile::Builder::new()
                    .prefix("image-")
                    .tempdir_in(directory)?;
                let tag = format!(
                    "seedbox-build:{}",
                    tag_directory
                        .path()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                );
                if let Some((evidence, name)) = context_options {
                    evidence.build_identity(
                        name,
                        &tag,
                        &auxiliary["build-context"],
                        &auxiliary["dockerfile"],
                    )?;
                }
                let build = (|| -> Result<()> {
                    let mut cmd = Command::new(&self.docker);
                    if local_builder {
                        cmd.arg("--remote=false");
                    }
                    cmd.args([
                        "build",
                        "--platform",
                        architecture.platform(),
                        "--tag",
                        &tag,
                        "--file",
                    ])
                    .arg(frozen_context.join(relative));
                    if let Some(epoch) = self.build_epoch {
                        cmd.arg(format!("--source-date-epoch={epoch}"))
                            .arg("--rewrite-timestamp");
                    }
                    cmd.arg(frozen_context);
                    run(&mut cmd, "build", Some(&tag), true, false)?;
                    let mut save = Command::new(&self.docker);
                    if local_builder {
                        save.arg("--remote=false");
                    }
                    save.args(["save", "--output"]).arg(&archive).arg(&tag);
                    run(&mut save, "save", Some(&tag), false, false)?;
                    Ok(())
                })();
                if let Some((evidence, _)) = context_options {
                    // Cancelled build cessation is not yet qualified. Preserve the tag;
                    // save cancellation occurs after build completion and permits cleanup.
                    if matches!(&build, Err(Error::PreparationCleanupUnverified(_))) {
                        return build
                            .and_then(|_| Err(Error::Execution("unreachable build state".into())));
                    }
                    let mut cleanup = Command::new(&self.docker);
                    cleanup.args(["--remote=false", "image", "rm", "--no-prune", &tag]);
                    run(&mut cleanup, "tag-cleanup", Some(&tag), false, true)?;
                    build?;
                    evidence.check()?;
                } else {
                    let mut cleanup = Command::new(&self.docker);
                    if local_builder {
                        cleanup.arg("--remote=false");
                    }
                    let cleanup = cleanup
                        .args(["image", "rm", "--no-prune", &tag])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    build?;
                    if !cleanup?.success() {
                        return Err(Error::Execution(
                            "could not remove private build tag".into(),
                        ));
                    }
                }
                format!("docker-archive:{}", archive.display())
            }
        };
        run(
            Command::new(&self.skopeo)
                .args([
                    "--insecure-policy",
                    "--override-os",
                    "linux",
                    "--override-arch",
                    architecture.oci_name(),
                    "copy",
                    "--preserve-digests",
                ])
                .arg(&source)
                .arg(format!("dir:{}", raw.display())),
            "copy",
            None,
            false,
            false,
        )?;
        let image = import_metadata_for_arch(&raw, service, store, architecture)?;
        provenance.insert(
            "resolved-manifest".into(),
            format!("sha256:{}", image.manifest.hex()),
        );
        run(
            Command::new(&self.skopeo)
                .args(["--insecure-policy", "copy", "--format", "oci"])
                .arg(format!("dir:{}", raw.display()))
                .arg(format!("oci:{}:seedbox", layout.display())),
            "normalize",
            None,
            false,
            false,
        )?;
        // umoci performs the extraction rather than custom host-side tar unpacking.
        // Root preserves image UID/GID/modes; rootless remapping would change the
        // application's filesystem semantics and is intentionally not used.
        run(
            Command::new(&self.umoci)
                .args(["unpack", "--image"])
                .arg(format!("{}:seedbox", layout.display()))
                .arg(&bundle),
            "unpack",
            None,
            false,
            false,
        )?;
        Ok(ImportedImage {
            image,
            rootfs: RootFilesystem::Directory(bundle.join("rootfs")),
            auxiliary,
            provenance,
        })
    }
}

/// Verify a single-platform manifest and its complete compressed input closure.
/// Kept separate from transport/extraction for testability and OCI producers.
pub fn import_metadata(
    raw: &Path,
    service: &Service,
    store: &ContentStore,
) -> Result<PreparedImage> {
    import_metadata_for_arch(raw, service, store, crate::guest::Architecture::Amd64)
}

pub fn import_metadata_for_arch(
    raw: &Path,
    service: &Service,
    store: &ContentStore,
    architecture: crate::guest::Architecture,
) -> Result<PreparedImage> {
    let manifest_bytes = fs::read(raw.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    if manifest.schema_version != 2 {
        return Err(Error::Integrity(
            "expected OCI/Docker schema 2 manifest".into(),
        ));
    }
    if ![
        "application/vnd.oci.image.config.v1+json",
        "application/vnd.docker.container.image.v1+json",
    ]
    .contains(&manifest.config.media_type.as_str())
    {
        return Err(Error::Integrity(
            "unsupported image configuration media type".into(),
        ));
    }
    let config_digest = retain_descriptor(raw, &manifest.config, store)?;
    let config: ImageConfig = serde_json::from_slice(&store.read(&config_digest)?)?;
    if config.os != "linux" || config.architecture != architecture.oci_name() {
        return Err(Error::Integrity(format!(
            "image must be {}",
            architecture.platform()
        )));
    }
    for field in ["OnBuild", "Healthcheck"] {
        if let Some(value) = config.config.get(field) {
            if !value.is_null() && value.as_array().is_none_or(|v| !v.is_empty()) {
                return Err(Error::Integrity(format!(
                    "image config {field} is unsupported"
                )));
            }
        }
    }
    if config.config.get("ArgsEscaped").and_then(|v| v.as_bool()) == Some(true) {
        return Err(Error::Integrity(
            "image config ArgsEscaped is unsupported".into(),
        ));
    }
    let defaults: ImageDefaults = serde_json::from_value(config.config)?;
    let launch = resolve_launch(service, &defaults)?;
    let mut layers = Vec::new();
    for layer in &manifest.layers {
        if ![
            "application/vnd.oci.image.layer.v1.tar",
            "application/vnd.oci.image.layer.v1.tar+gzip",
            "application/vnd.oci.image.layer.v1.tar+zstd",
            "application/vnd.docker.image.rootfs.diff.tar.gzip",
        ]
        .contains(&layer.media_type.as_str())
        {
            return Err(Error::Integrity(format!(
                "unsupported layer media type {}",
                layer.media_type
            )));
        }
        layers.push(retain_descriptor(raw, layer, store)?);
    }
    Ok(PreparedImage {
        manifest: store.put(&manifest_bytes)?,
        config: config_digest,
        layers,
        launch,
    })
}

fn retain_descriptor(
    raw: &Path,
    descriptor: &Descriptor,
    store: &ContentStore,
) -> Result<ContentDigest> {
    let hex = descriptor
        .digest
        .strip_prefix("sha256:")
        .ok_or_else(|| Error::Integrity("only sha256 OCI descriptors are supported".into()))?;
    let expected = ContentDigest::try_from(hex.to_owned())?;
    let path = raw.join(expected.hex());
    if !fs::symlink_metadata(&path)?.file_type().is_file()
        || fs::metadata(&path)?.len() != descriptor.size
    {
        return Err(Error::Integrity(format!(
            "descriptor size/type mismatch: {}",
            descriptor.digest
        )));
    }
    let actual = store.put_file(&path)?;
    if actual != expected {
        return Err(Error::Integrity(format!(
            "descriptor hash mismatch: {}",
            descriptor.digest
        )));
    }
    Ok(actual)
}

fn snapshot_context(root: &Path, destination: &Path) -> Result<()> {
    let mut archive = tar::Builder::new(fs::File::create(destination)?);
    for item in walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .follow_links(false)
        .min_depth(1)
    {
        let item = item.map_err(|e| Error::Execution(e.to_string()))?;
        let metadata = item.path().symlink_metadata()?;
        if !(metadata.is_file() || metadata.is_dir() || metadata.file_type().is_symlink()) {
            return Err(Error::Execution(format!(
                "unsupported special file in build context: {}",
                item.path().display()
            )));
        }
        let name = item.path().strip_prefix(root).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&metadata);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if metadata.file_type().is_symlink() {
            header.set_size(0);
            archive.append_link(&mut header, name, fs::read_link(item.path())?)?;
        } else if metadata.is_dir() {
            header.set_size(0);
            archive.append_data(&mut header, name, io::empty())?;
        } else {
            archive.append_data(&mut header, name, fs::File::open(item.path())?)?;
        }
    }
    archive.finish()?;
    Ok(())
}

fn version(path: &Path) -> Result<String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| spawn_error(path.as_os_str(), error))?;
    if !output.status.success() {
        return Err(Error::Execution(format!(
            "{} --version failed",
            path.display()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

/// Name the program in launch failures; a bare ENOENT does not say what is missing.
pub(crate) fn spawn_error(program: &std::ffi::OsStr, error: io::Error) -> Error {
    let program = Path::new(program).display();
    if error.kind() == io::ErrorKind::NotFound {
        Error::Setup(format!(
            "{program} not found; install it or enter `nix develop`"
        ))
    } else {
        Error::Execution(format!("running {program}: {error}"))
    }
}

pub(crate) fn checked(command: &mut Command, log: &Path) -> Result<()> {
    let file = fs::File::create(log)?;
    let status = command
        .stdout(file.try_clone()?)
        .stderr(file)
        .status()
        .map_err(|error| spawn_error(command.get_program(), error))?;
    if !status.success() {
        let mut tail = String::new();
        fs::File::open(log)?.take(8192).read_to_string(&mut tail)?;
        return Err(Error::Execution(format!(
            "command {:?} failed ({status}): {tail}",
            command.get_program()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn failed_build_cleans_only_its_private_tag_without_pruning_in_both_paths() {
        // These are recorded host-process fixtures, never a native image build.
        for (owned, epoch) in [
            (false, None),
            (true, None),
            (false, Some(0)),
            (true, Some(123)),
        ] {
            let root = tempfile::tempdir().unwrap();
            let context = root.path().join("context");
            fs::create_dir(&context).unwrap();
            fs::write(context.join("Dockerfile"), "FROM scratch\n").unwrap();
            let argv = root.path().join("cleanup-argv");
            let build_argv = root.path().join("build-argv");
            let builder = root.path().join("builder");
            fs::write(&builder, format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture-v1; exit 0; fi\nif [ \"$1\" = --remote=false ]; then shift; fi\ncase \"$1\" in build) printf '%s\\n' \"$@\" > '{}'; exit 7;; image) printf '%s\\n' \"$@\" > '{}'; exit 0;; *) exit 9;; esac\n", build_argv.display(), argv.display())).unwrap();
            fs::set_permissions(&builder, fs::Permissions::from_mode(0o755)).unwrap();
            let transport = root.path().join("transport");
            fs::write(
                &transport,
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture-v1; exit 0; fi\nexit 9\n",
            )
            .unwrap();
            fs::set_permissions(&transport, fs::Permissions::from_mode(0o755)).unwrap();
            let simulation = crate::config::parse(
                "name: cleanup\nservices:\n  app: {build: ./context}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n",
                &root.path().join("compose.yaml"), &BTreeMap::new()).unwrap();
            let tools = OciTools {
                docker: builder,
                skopeo: transport.clone(),
                umoci: transport,
                build_epoch: epoch,
            };
            let evidence = crate::preparation::PreparationContext::new(
                root.path().join("evidence"),
                crate::execution::CancellationToken::default(),
            )
            .unwrap();
            let result = tools.import_with_context(
                &simulation.services["app"],
                &root.path().join("import"),
                &ContentStore::new(root.path().join("store")),
                owned.then_some((&evidence, "app")),
            );
            let error = result.err().unwrap();
            assert!(matches!(error, Error::Execution(_)));
            assert!(error.to_string().contains('7'));
            let build = fs::read_to_string(&build_argv).unwrap();
            assert_eq!(build.contains("--rewrite-timestamp"), epoch.is_some());
            if let Some(epoch) = epoch {
                assert!(build
                    .lines()
                    .any(|arg| arg == format!("--source-date-epoch={epoch}")));
            }
            let actual = fs::read_to_string(&argv).unwrap();
            let args: Vec<_> = actual.lines().collect();
            assert_eq!(&args[..3], ["image", "rm", "--no-prune"]);
            assert_eq!(args.len(), 4);
            assert!(args[3].starts_with("seedbox-build:image-"));
            if owned {
                let receipt: serde_json::Value = serde_json::from_slice(
                    &fs::read(root.path().join("evidence/app-tag-cleanup.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    receipt["argv"],
                    serde_json::json!(["--remote=false", "image", "rm", "--no-prune", args[3]])
                );
                assert_eq!(receipt["private_tag"], args[3]);
                assert_eq!(receipt["reaped"], true);
            }
        }
    }

    #[test]
    fn frozen_context_retains_bytes_and_metadata_and_invalidates_changed_sources() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("Dockerfile"), "FROM scratch\nCOPY . /app\n").unwrap();
        fs::write(source.join("app"), b"first").unwrap();
        fs::write(source.join(".dockerignore"), "ignored\n").unwrap();
        fs::write(
            source.join("ignored"),
            b"retained for the builder to filter",
        )
        .unwrap();
        symlink("app", source.join("link")).unwrap();
        let store = ContentStore::new(temp.path().join("store"));
        let capture =
            |name: &str| FrozenContext::capture(&source, &temp.path().join(name), &store).unwrap();
        let initial = capture("initial");
        assert_eq!(initial.digest, capture("same").digest);
        fs::write(source.join("app"), b"second").unwrap();
        assert_eq!(fs::read(initial.root.join("app")).unwrap(), b"first");
        let changed_bytes = capture("bytes");
        assert_ne!(initial.digest, changed_bytes.digest);
        fs::set_permissions(source.join("app"), fs::Permissions::from_mode(0o751)).unwrap();
        let changed_mode = capture("mode");
        assert_ne!(changed_bytes.digest, changed_mode.digest);
        assert_eq!(
            fs::metadata(changed_mode.root.join("app"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        fs::remove_file(source.join("link")).unwrap();
        symlink("Dockerfile", source.join("link")).unwrap();
        let changed_link = capture("link");
        assert_ne!(changed_mode.digest, changed_link.digest);
        fs::write(source.join(".dockerignore"), "other\n").unwrap();
        let changed_ignore = capture("ignore");
        assert_ne!(changed_link.digest, changed_ignore.digest);
        fs::write(source.join("Dockerfile"), "FROM scratch\nCOPY app /other\n").unwrap();
        assert_ne!(changed_ignore.digest, capture("dockerfile").digest);
        store.verify(&initial.digest).unwrap();
    }

    #[test]
    fn shared_image_resolves_each_services_launch_without_changing_retained_image_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(temp.path().join("store"));
        let config = store.put(br#"{"os":"linux","architecture":"amd64","config":{"Entrypoint":["/app"],"Cmd":["default"],"Env":["KEEP=1","REMOVE=1"],"WorkingDir":"/work","User":"10:20","Volumes":{"/data":{}}}}"#).unwrap();
        let simulation = crate::config::parse("name: rebind\nservices:\n  a:\n    image: example:v1\n    command: [first]\n  b:\n    image: example:v1\n    entrypoint: [/other]\n    command: [second]\n    environment: {KEEP: different, REMOVE: null}\n    working_dir: /other-work\n    user: '30:40'\nx-seedbox:\n  version: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: b}\n",Path::new("/compose.yaml"), &BTreeMap::new()).unwrap();
        let defaults: ImageDefaults = serde_json::from_value(
            serde_json::from_slice::<ImageConfig>(&store.read(&config).unwrap())
                .unwrap()
                .config,
        )
        .unwrap();
        let imported = ImportedImage {
            image: PreparedImage {
                manifest: store.put(b"manifest").unwrap(),
                config,
                layers: vec![store.put(b"layer").unwrap()],
                launch: resolve_launch(&simulation.services["a"], &defaults).unwrap(),
            },
            rootfs: RootFilesystem::Directory(temp.path().join("rootfs")),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        let rebound = imported
            .for_service(&simulation.services["b"], &store)
            .unwrap();
        assert_eq!(imported.image.launch.argv, ["/app", "first"]);
        assert_eq!(rebound.image.launch.argv, ["/other", "second"]);
        assert_eq!(
            rebound.image.launch.environment,
            BTreeMap::from([("KEEP".into(), "different".into())])
        );
        assert_eq!(rebound.image.launch.working_dir, "/other-work");
        assert_eq!(rebound.image.launch.user, "30:40");
        assert_eq!(rebound.image.launch.volumes, ["/data"]);
        assert_eq!(rebound.image.manifest, imported.image.manifest);
        assert_eq!(rebound.image.config, imported.image.config);
        assert_eq!(rebound.image.layers, imported.image.layers);
        assert_eq!(rebound.rootfs, imported.rootfs);
    }
}
