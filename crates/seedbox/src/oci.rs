//! Validate and retain original OCI image descriptors and launch configuration.
use crate::{
    config::Service,
    execution::PreparedImage,
    image::{resolve_launch, ImageDefaults},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
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
        architecture: crate::guest::Architecture,
    ) -> Result<ImportedImage>;
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
