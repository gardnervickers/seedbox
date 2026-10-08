//! Container-backed image transport. Only filesystem tools execute on Linux;
//! configuration, identity, validation and boot archive assembly stay in Seedbox.
use crate::{
    config::{ImageSource, Service, Simulation},
    execution::{CancellationToken, PreparationBackend, PreparedInputs},
    guest::Architecture,
    oci::{FrozenContext, ImageTransport, ImportedImage, RootFilesystem},
    preparation::{OciPreparer, PreparationContext},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

include!(concat!(env!("OUT_DIR"), "/container-runtime.rs"));
const REQUIRED: [&str; 7] = [
    "Image",
    "compose-init",
    "busybox",
    "mke2fs",
    "tools-image.tar",
    "NOTICE.txt",
    "provenance.json",
];
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    version: u32,
    image: String,
    #[serde(deserialize_with = "crate::runtime_assets::unique_map")]
    files: BTreeMap<String, ContentDigest>,
}
#[derive(Clone, Debug)]
pub struct Runtime {
    pub root: PathBuf,
    manifest: Manifest,
}
impl Runtime {
    pub fn available() -> bool {
        RUNTIME.is_some()
    }
    pub fn embedded(cache: &Path, kernel_only: bool) -> Result<Self> {
        let bytes = RUNTIME.ok_or_else(|| {
            Error::Setup(
                "guest assets are absent; run cargo build --release and use target/release/seedbox"
                    .into(),
            )
        })?;
        Self::install(bytes, cache, kernel_only)
    }
    pub fn install(bytes: &[u8], cache: &Path, kernel_only: bool) -> Result<Self> {
        fs::create_dir_all(cache)?;
        let digest = ContentDigest::of(bytes);
        let root = cache.join(format!(
            "{}{}",
            digest.hex(),
            if kernel_only { "-kernel" } else { "" }
        ));
        if !root.exists() {
            let stage = tempfile::tempdir_in(cache)?;
            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
            let mut names = std::collections::BTreeSet::new();
            let mut size = 0u64;
            for entry in archive.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                let name = path
                    .to_str()
                    .ok_or_else(|| Error::Integrity("non-UTF-8 runtime member".into()))?;
                if !entry.header().entry_type().is_file()
                    || !valid_member(name)
                    || !names.insert(name.to_owned())
                {
                    return Err(Error::Integrity(
                        "invalid or duplicate runtime member".into(),
                    ));
                }
                size = size
                    .checked_add(entry.size())
                    .filter(|size| *size <= 2 << 30)
                    .ok_or_else(|| Error::Integrity("runtime exceeds 2 GiB".into()))?;
                if !kernel_only || name == "Image" || name == "runtime.json" {
                    entry.unpack(stage.path().join(name))?;
                }
            }
            Self::validate(stage.path(), kernel_only)?;
            match fs::rename(stage.path(), &root) {
                Ok(()) => (),
                Err(_) if root.is_dir() => (),
                Err(error) => return Err(error.into()),
            }
        }
        let manifest = Self::validate(&root, kernel_only)?;
        Ok(Self { root, manifest })
    }
    fn validate(root: &Path, kernel_only: bool) -> Result<Manifest> {
        if !fs::symlink_metadata(root)?.is_dir()
            || !fs::symlink_metadata(root.join("runtime.json"))?.is_file()
        {
            return Err(Error::Integrity("runtime cache is a symlink".into()));
        }
        let manifest: Manifest =
            serde_json::from_reader(fs::File::open(root.join("runtime.json"))?.take(1 << 20))?;
        if manifest.version != 1
            || !manifest.image.starts_with("sha256:")
            || manifest.image.len() != 71
            || !manifest.image[7..].bytes().all(|c| c.is_ascii_hexdigit())
            || REQUIRED
                .iter()
                .any(|name| !manifest.files.contains_key(*name))
        {
            return Err(Error::Integrity("invalid runtime manifest".into()));
        }
        for (name, digest) in &manifest.files {
            if !valid_member(name) || name == "runtime.json" {
                return Err(Error::Integrity("invalid runtime manifest path".into()));
            }
            if kernel_only && name != "Image" {
                continue;
            }
            let path = root.join(name);
            if !fs::symlink_metadata(&path)?.is_file() || package_digest(&path)? != *digest {
                return Err(Error::Integrity(format!(
                    "runtime {name} hash mismatch; remove cache {} and retry",
                    root.display()
                )));
            }
        }
        Ok(manifest)
    }
    pub fn input(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

fn valid_member(name: &str) -> bool {
    REQUIRED.contains(&name)
        || [
            "runtime.json",
            "kernel.config",
            "counter-audit.txt",
            "bzImage",
            "System.map",
            "SHA256SUMS",
        ]
        .contains(&name)
}

fn package_digest(path: &Path) -> Result<ContentDigest> {
    use sha2::{Digest, Sha256};
    let mut reader = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    ContentDigest::try_from(format!("{:x}", hash.finalize()))
}

#[derive(Clone, Debug)]
pub struct ContainerPreparer {
    pub inputs: OciPreparer,
    pub runtime: Runtime,
    pub client: PathBuf,
    pub cancellation: CancellationToken,
    pub progress: fn(&str),
}
impl ContainerPreparer {
    fn command(&self) -> Command {
        Command::new(&self.client)
    }
    fn run(
        &self,
        context: &PreparationContext,
        command: &mut Command,
        phase: &str,
        cleanup: bool,
    ) -> Result<()> {
        if !cleanup {
            (self.progress)(phase);
        }
        context.run(command, phase, None, false, cleanup)
    }
    /// Probe an explicit client through the owned, bounded command transport.
    pub fn check_client(client: &Path, cancellation: CancellationToken) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let context = PreparationContext::new(directory.path().join("evidence"), cancellation)?;
        let version = context.version(client, "client-version")?;
        let format = if version.to_ascii_lowercase().contains("podman") {
            "{{.Host.OS}}"
        } else {
            "{{.OSType}}"
        };
        let output = context.output(Command::new(client).args(["info", "--format", format]), "engine-info", false)
            .map_err(|error| match error {
                Error::PreparationCancelled => error,
                _ => Error::Setup(format!("container engine unavailable through {}; start Docker Desktop or your Podman machine (Linux: start its service), and check DOCKER_HOST or CONTAINER_HOST; run {} info for details", client.display(), client.display())),
            })?;
        if String::from_utf8_lossy(&output).trim() != "linux" {
            return Err(Error::Setup(
                "container engine must run Linux containers; switch Docker to Linux containers"
                    .into(),
            ));
        }
        Ok(())
    }
    fn remove_helper(
        &self,
        context: &PreparationContext,
        container: &str,
        phase: &str,
    ) -> Result<()> {
        if self
            .run(
                context,
                self.command().args(["rm", "--force", container]),
                phase,
                true,
            )
            .is_ok()
        {
            return Ok(());
        }
        // A cancelled create may have succeeded on the server or may never
        // have created anything. Confirm absence before declaring cleanup done.
        let output = context
            .output(
                self.command()
                    .args(["ps", "--all", "--format", "{{.Names}}"]),
                &format!("{phase}-verify"),
                true,
            )
            .map_err(|_| {
                Error::PreparationCleanupUnverified(format!(
                    "could not verify removal of private helper {container}; inspect {} ps --all",
                    self.client.display()
                ))
            })?;
        if String::from_utf8_lossy(&output)
            .split(|c: char| c.is_whitespace() || c == ',')
            .any(|name| name == container)
        {
            return Err(Error::PreparationCleanupUnverified(format!(
                "private helper {container} remains; remove it with {} rm --force {container}",
                self.client.display()
            )));
        }
        Ok(())
    }
    fn ensure_tools(&self, context: &PreparationContext) -> Result<()> {
        let inspect = |phase: &str| -> Result<Vec<u8>> {
            context.output(
                self.command().args([
                    "image",
                    "inspect",
                    "--format",
                    "{{.Id}}",
                    &self.runtime.manifest.image,
                ]),
                phase,
                false,
            )
        };
        match inspect("filesystem-tools-present") {
            Ok(_) => return Ok(()),
            Err(Error::PreparationCancelled) => return Err(Error::PreparationCancelled),
            Err(_) => (),
        }
        self.run(
            context,
            self.command()
                .args(["load", "--input"])
                .arg(self.runtime.input("tools-image.tar")),
            "loading-filesystem-tools",
            false,
        )?;
        let id = inspect("filesystem-tools-loaded")?;
        if String::from_utf8_lossy(&id)
            .trim()
            .trim_start_matches("sha256:")
            != &self.runtime.manifest.image[7..]
        {
            return Err(Error::Integrity(
                "container engine did not load the bundled filesystem tools".into(),
            ));
        }
        Ok(())
    }
}
impl PreparationBackend for ContainerPreparer {
    fn validate_runtime_inputs(&self) -> Result<()> {
        self.validate_inputs()?;
        Self::check_client(&self.client, self.cancellation.clone())
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        let diagnostics = tempfile::tempdir()?;
        let context = PreparationContext::new(
            diagnostics.path().join("evidence"),
            self.cancellation.clone(),
        )?;
        let result = self.prepare_cancellable(simulation, store, &context);
        match result {
            Err(Error::PreparationCancelled) => Err(Error::PreparationCancelled),
            Err(Error::PreparationCleanupUnverified(detail)) => {
                Err(Error::PreparationCleanupUnverified(format!(
                    "{detail}; preparation evidence: {}",
                    diagnostics.keep().display()
                )))
            }
            Err(error) => Err(Error::Execution(format!(
                "{error}; preparation evidence: {}",
                diagnostics.keep().display()
            ))),
            value => value,
        }
    }
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        context.check()?;
        self.validate_inputs()?;
        Self::check_client(&self.client, context.cancellation.clone())?;
        self.ensure_tools(context)?;
        self.inputs
            .prepare_with_transport(simulation, store, context, self)
    }
}
impl ContainerPreparer {
    fn validate_inputs(&self) -> Result<()> {
        Self::validate_assets(&self.inputs)?;
        if self.inputs.tools.build_epoch.is_some() {
            return Err(Error::Setup("--build-epoch requires native Linux Podman preparation; omit it for container-backed preparation".into()));
        }
        Ok(())
    }
    fn validate_assets(inputs: &OciPreparer) -> Result<()> {
        let arch = Architecture::from_profile(&inputs.profile)?;
        arch.validate_elf(&inputs.init)?;
        arch.validate_elf(&inputs.busybox)?;
        if let Some(path) = &inputs.mke2fs {
            arch.validate_elf(path)?;
        }
        if crate::admission::file_digest(&inputs.engine)? != inputs.profile.engine {
            return Err(Error::Integrity(
                "executable changed after profile construction".into(),
            ));
        }
        Ok(())
    }
}
impl ImageTransport for ContainerPreparer {
    fn import(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context: Option<(&PreparationContext, &str)>,
        frozen: Option<&FrozenContext>,
        architecture: Architecture,
    ) -> Result<ImportedImage> {
        let (context, service_name) = context.ok_or_else(|| {
            Error::Integrity("container preparation requires an owned context".into())
        })?;
        fs::create_dir_all(directory)?;
        let private = tempfile::Builder::new()
            .prefix("seedbox-image-")
            .tempdir()?;
        let name = private
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::Integrity("invalid private name".into()))?;
        let tag = format!("seedbox-build:{name}");
        let container = format!("seedbox-prep-{name}");
        let mut auxiliary = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        let owned;
        let reference = match &service.source {
            ImageSource::Registry { reference } => {
                self.run(
                    context,
                    self.command()
                        .args(["pull", "--platform", architecture.platform(), reference]),
                    &format!("{service_name}-pulling-image"),
                    false,
                )?;
                reference.clone()
            }
            ImageSource::Build {
                context: source,
                dockerfile,
            } => {
                let relative = dockerfile
                    .strip_prefix(source)
                    .map_err(|_| Error::Integrity("Dockerfile escaped build context".into()))?;
                let frozen = match frozen {
                    Some(value) => value,
                    None => {
                        owned = FrozenContext::capture(source, private.path(), store)?;
                        &owned
                    }
                };
                auxiliary.insert("build-context".into(), frozen.digest.clone());
                auxiliary.insert(
                    "dockerfile".into(),
                    store.put_file(&frozen.root.join(relative))?,
                );
                let result = self.run(
                    context,
                    self.command()
                        .args([
                            "build",
                            "--platform",
                            architecture.platform(),
                            "--tag",
                            &tag,
                            "--file",
                        ])
                        .arg(frozen.root.join(relative))
                        .arg(&frozen.root),
                    &format!("{service_name}-building-image"),
                    false,
                );
                if matches!(result, Err(Error::PreparationCancelled)) {
                    return Err(Error::PreparationCleanupUnverified(format!("container-engine build cancellation cannot confirm daemon cessation; private tag {tag} retained; inspect {} ps and {} images", self.client.display(), self.client.display())));
                }
                result?;
                tag.clone()
            }
        };
        let operation = (|| {
            // Resolve the mutable tag once. Export exact image bytes, even when
            // another client updates that tag while this preparation is running.
            let inspected = context.output(
                self.command()
                    .args(["image", "inspect", "--format", "{{.Id}}", &reference]),
                &format!("{service_name}-resolved-image"),
                false,
            )?;
            let image_id = String::from_utf8_lossy(&inspected).trim().to_owned();
            self.run(
                context,
                self.command()
                    .args(["save", "--output"])
                    .arg(private.path().join("image.tar"))
                    .arg(&image_id),
                &format!("{service_name}-exporting-image"),
                false,
            )?;
            provenance.insert("container-image".into(), image_id);
            provenance.insert("container-client".into(), self.client.display().to_string());
            let filesystem = (|| {
                self.run(
                    context,
                    self.command().args([
                        "create",
                        "--name",
                        &container,
                        "--network",
                        "none",
                        "--user",
                        "0",
                        &self.runtime.manifest.image,
                    ]),
                    &format!("{service_name}-creating-filesystem-helper"),
                    false,
                )?;
                self.run(
                    context,
                    self.command().args(["start", &container]),
                    &format!("{service_name}-starting-filesystem-helper"),
                    false,
                )?;
                self.run(
                    context,
                    self.command()
                        .args(["exec", &container, "/tools/bin/mkdir", "/work"]),
                    &format!("{service_name}-preparing-filesystem"),
                    false,
                )?;
                self.run(
                    context,
                    self.command()
                        .arg("cp")
                        .arg(private.path().join("image.tar"))
                        .arg(format!("{container}:/work/image.tar")),
                    &format!("{service_name}-copying-image"),
                    false,
                )?;
                self.run(
                    context,
                    self.command().args([
                        "exec",
                        &container,
                        "/tools/bin/python3",
                        "/filesystem.py",
                        "/work",
                    ]),
                    &format!("{service_name}-unpacking-filesystem"),
                    false,
                )?;
                self.run(
                    context,
                    self.command()
                        .arg("cp")
                        .arg(format!("{container}:/work/source"))
                        .arg(directory.join("source")),
                    &format!("{service_name}-copying-metadata"),
                    false,
                )?;
                self.run(
                    context,
                    self.command()
                        .arg("cp")
                        .arg(format!("{container}:/work/rootfs.tar"))
                        .arg(directory.join("rootfs.tar")),
                    &format!("{service_name}-copying-filesystem"),
                    false,
                )?;
                let image = crate::oci::import_metadata_for_arch(
                    &directory.join("source"),
                    service,
                    store,
                    architecture,
                )?;
                Ok(ImportedImage {
                    image,
                    rootfs: RootFilesystem::Archive(directory.join("rootfs.tar")),
                    auxiliary: auxiliary.clone(),
                    provenance: provenance.clone(),
                })
            })();
            self.remove_helper(
                context,
                &container,
                &format!("{service_name}-helper-cleanup"),
            )?;
            filesystem
        })();
        if matches!(service.source, ImageSource::Build { .. }) {
            self.run(
                context,
                self.command().args(["image", "rm", "--no-prune", &tag]),
                &format!("{service_name}-tag-cleanup"),
                true,
            )?;
        }
        operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn package(extra: Option<&str>) -> Vec<u8> {
        let files: BTreeMap<String, ContentDigest> = REQUIRED
            .into_iter()
            .map(|name| (name.into(), ContentDigest::of(name.as_bytes())))
            .collect();
        let manifest = Manifest {
            version: 1,
            image: format!("sha256:{}", "a".repeat(64)),
            files,
        };
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in REQUIRED
            .into_iter()
            .map(|name| (name.to_owned(), name.as_bytes().to_vec()))
            .chain(std::iter::once((
                "runtime.json".into(),
                serde_json::to_vec(&manifest).unwrap(),
            )))
            .chain(extra.map(|name| (name.into(), b"unexpected".to_vec())))
        {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o600);
            header.set_size(data.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, name, data.as_slice()).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&raw).unwrap();
        gzip.finish().unwrap()
    }
    #[test]
    fn verified_cache_detects_replacement_and_doctor_extracts_only_kernel() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = package(None);
        let kernel = Runtime::install(&bytes, directory.path(), true).unwrap();
        assert!(kernel.input("Image").is_file());
        assert!(!kernel.input("tools-image.tar").exists());
        assert!(!kernel.input("compose-init").exists());
        let all = Runtime::install(&bytes, directory.path(), false).unwrap();
        assert!(all.input("tools-image.tar").is_file());
        fs::write(all.input("Image"), "replacement").unwrap();
        assert!(Runtime::install(&bytes, directory.path(), false)
            .unwrap_err()
            .to_string()
            .contains("hash mismatch"));
    }
    #[test]
    fn unexpected_members_never_publish_a_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let error =
            Runtime::install(&package(Some("unexpected")), directory.path(), false).unwrap_err();
        assert!(error
            .to_string()
            .contains("invalid or duplicate runtime member"));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        for name in ["/", "/tmp/escaped", "..", "../Image", "subdirectory/Image"] {
            assert!(!valid_member(name));
        }
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn client_probe_checks_linux_and_preserves_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let client = directory.path().join("client");
        for (reply, expected) in [
            ("printf linux", None),
            ("printf windows", Some("must run Linux containers")),
            ("exit 1", Some("container engine unavailable")),
        ] {
            fs::write(
                &client,
                format!(
                    "#!/bin/sh\nif test \"$1\" = --version; then printf podman; else {reply}; fi\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&client, fs::Permissions::from_mode(0o755)).unwrap();
            let result = ContainerPreparer::check_client(&client, Default::default());
            match expected {
                None => assert!(result.is_ok(), "{result:?}"),
                Some(message) => assert!(result.unwrap_err().to_string().contains(message)),
            }
        }
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        assert!(matches!(
            ContainerPreparer::check_client(&client, cancellation),
            Err(Error::PreparationCancelled)
        ));
    }
    #[test]
    fn interrupted_create_requires_confirmed_absence_before_cleanup_is_successful() {
        let directory = tempfile::tempdir().unwrap();
        let client = directory.path().join("client");
        let runtime = Runtime {
            root: directory.path().into(),
            manifest: Manifest {
                version: 1,
                image: format!("sha256:{}", "a".repeat(64)),
                files: BTreeMap::new(),
            },
        };
        let inputs = OciPreparer {
            tools: Default::default(),
            kernel: "kernel".into(),
            engine: "engine".into(),
            init: "init".into(),
            busybox: "busybox".into(),
            mke2fs: None,
            profile: crate::execution::ExecutionProfile {
                backend: "fixture".into(),
                witness_mode: crate::execution::WitnessMode::Exits,
                engine: ContentDigest::of(b"engine"),
                semantic_parameters: BTreeMap::new(),
            },
        };
        let preparer = ContainerPreparer {
            inputs,
            runtime,
            client: client.clone(),
            cancellation: Default::default(),
            progress: |_| {},
        };
        for (remaining, success) in [("", true), ("seedbox-private", false)] {
            fs::write(&client, format!("#!/bin/sh\nif test \"$1\" = rm; then exit 1; fi\nprintf '%s\\n' '{remaining}'\n")).unwrap();
            fs::set_permissions(&client, fs::Permissions::from_mode(0o755)).unwrap();
            let evidence = tempfile::tempdir().unwrap();
            let context =
                PreparationContext::new(evidence.path().join("evidence"), Default::default())
                    .unwrap();
            let result = preparer.remove_helper(&context, "seedbox-private", "cleanup");
            assert_eq!(result.is_ok(), success);
            if !success {
                assert!(matches!(
                    result,
                    Err(Error::PreparationCleanupUnverified(_))
                ));
            }
        }
    }
}
