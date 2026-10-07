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

#[derive(Clone, Debug)]
pub struct OciTools {
    pub skopeo: PathBuf,
    pub umoci: PathBuf,
    pub docker: PathBuf,
}

impl Default for OciTools {
    fn default() -> Self {
        Self {
            skopeo: "skopeo".into(),
            umoci: "umoci".into(),
            docker: "podman".into(),
        }
    }
}

pub struct ImportedImage {
    pub image: PreparedImage,
    pub rootfs: PathBuf,
    pub auxiliary: BTreeMap<String, ContentDigest>,
    pub provenance: BTreeMap<String, String>,
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
        fs::create_dir_all(directory)?;
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
        provenance.insert("skopeo".into(), version(&self.skopeo)?);
        provenance.insert("umoci".into(), version(&self.umoci)?);
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
                let snapshot = directory.join("context.tar");
                snapshot_context(context, &snapshot)?;
                auxiliary.insert("build-context".into(), store.put_file(&snapshot)?);
                let frozen_context = directory.join("build-context");
                fs::create_dir(&frozen_context)?;
                tar::Archive::new(fs::File::open(&snapshot)?).unpack(&frozen_context)?;
                auxiliary.insert(
                    "dockerfile".into(),
                    store.put_file(&frozen_context.join(relative))?,
                );
                provenance.insert("docker".into(), version(&self.docker)?);
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
                let build = (|| -> Result<()> {
                    let mut cmd = Command::new(&self.docker);
                    cmd.args([
                        "build",
                        "--platform",
                        "linux/amd64",
                        "--tag",
                        &tag,
                        "--file",
                    ])
                    .arg(frozen_context.join(relative))
                    .arg(&frozen_context);
                    checked(&mut cmd, &directory.join("build.log"))?;
                    checked(
                        Command::new(&self.docker)
                            .args(["save", "--output"])
                            .arg(&archive)
                            .arg(&tag),
                        &directory.join("save.log"),
                    )?;
                    Ok(())
                })();
                let cleanup = Command::new(&self.docker)
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
                format!("docker-archive:{}", archive.display())
            }
        };
        checked(
            Command::new(&self.skopeo)
                .args([
                    "--insecure-policy",
                    "--override-os",
                    "linux",
                    "--override-arch",
                    "amd64",
                    "copy",
                    "--preserve-digests",
                ])
                .arg(&source)
                .arg(format!("dir:{}", raw.display())),
            &directory.join("copy.log"),
        )?;
        let image = import_metadata(&raw, service, store)?;
        provenance.insert(
            "resolved-manifest".into(),
            format!("sha256:{}", image.manifest.hex()),
        );
        checked(
            Command::new(&self.skopeo)
                .args(["--insecure-policy", "copy", "--format", "oci"])
                .arg(format!("dir:{}", raw.display()))
                .arg(format!("oci:{}:seedbox", layout.display())),
            &directory.join("normalize.log"),
        )?;
        // umoci performs the extraction rather than custom host-side tar unpacking.
        // Root preserves image UID/GID/modes; rootless remapping would change the
        // application's filesystem semantics and is intentionally not used.
        checked(
            Command::new(&self.umoci)
                .args(["unpack", "--image"])
                .arg(format!("{}:seedbox", layout.display()))
                .arg(&bundle),
            &directory.join("unpack.log"),
        )?;
        Ok(ImportedImage {
            image,
            rootfs: bundle.join("rootfs"),
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
    if config.os != "linux" || config.architecture != "amd64" {
        return Err(Error::Integrity("image must be linux/amd64".into()));
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
fn spawn_error(program: &std::ffi::OsStr, error: io::Error) -> Error {
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
