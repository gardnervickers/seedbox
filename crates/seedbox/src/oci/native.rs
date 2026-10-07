//! Native, unprivileged import of completed Linux container images.

use super::{config_launch, filesystem, Descriptor, GuestPlatform, ImageConfig, Manifest};
use crate::{
    config::Service,
    execution::PreparedImage,
    preparation::PreparationContext,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

mod registry;
pub use registry::{RegistryClient, RegistryReference};

const META_LIMIT: u64 = 8 * 1024 * 1024;
const BLOB_LIMIT: u64 = 16 * 1024 * 1024 * 1024;
const ARCHIVE_LIMIT: u64 = 64 * 1024 * 1024 * 1024;
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_INDEX: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
const TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const DOCKER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

/// Transport selection is explicit. No import invokes a Dockerfile builder.
pub enum ImageInput<'a> {
    Registry(&'a str),
    OciLayout {
        path: &'a Path,
        reference: Option<&'a str>,
    },
    OciArchive {
        path: &'a Path,
        reference: Option<&'a str>,
    },
    DockerArchive {
        path: &'a Path,
        reference: Option<&'a str>,
    },
    Docker {
        client: &'a Path,
        reference: &'a str,
    },
    Podman {
        client: &'a Path,
        reference: &'a str,
    },
}

pub struct NativeImportedImage {
    pub image: PreparedImage,
    pub filesystem: filesystem::ImageFilesystem,
    pub auxiliary: BTreeMap<String, ContentDigest>,
    pub provenance: BTreeMap<String, String>,
}

/// Registry policy and clients are supplied by the caller. This library reads
/// no environment, credential files or CLI options.
#[derive(Default)]
pub struct NativeOci {
    pub registry: RegistryClient,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    schema_version: u32,
    media_type: Option<String>,
    manifests: Vec<IndexDescriptor>,
}

#[derive(Deserialize)]
struct IndexDescriptor {
    #[serde(flatten)]
    descriptor: Descriptor,
    platform: Option<Platform>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Platform {
    os: String,
    architecture: String,
    variant: Option<String>,
}

#[derive(Deserialize)]
struct RootfsConfig {
    rootfs: Rootfs,
}

#[derive(Deserialize)]
struct Rootfs {
    #[serde(rename = "type")]
    kind: String,
    diff_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SavedImage {
    config: String,
    #[serde(default)]
    repo_tags: Option<Vec<String>>,
    layers: Vec<String>,
}

enum Source {
    Layout(PathBuf),
    Archive(BTreeMap<String, ContentDigest>),
    Registry(Box<registry::Session>),
}

impl Source {
    fn saved(objects: BTreeMap<String, ContentDigest>) -> Self {
        Self::Archive(
            objects
                .into_values()
                .map(|digest| (format!("blobs/sha256/{}", digest.hex()), digest))
                .collect(),
        )
    }

    fn blob(
        &mut self,
        descriptor: &Descriptor,
        store: &ContentStore,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ContentDigest> {
        let expected = descriptor_digest(descriptor)?;
        descriptor_limit(descriptor)?;
        let actual = match self {
            Self::Layout(root) => {
                let path = format!("blobs/sha256/{}", expected.hex());
                let file = layout_file(root, &path)?;
                if file.metadata()?.len() != descriptor.size {
                    return Err(Error::Integrity(format!(
                        "descriptor size mismatch: {}",
                        descriptor.digest
                    )));
                }
                retain(file, store, descriptor.size, check)?
            }
            Self::Archive(objects) => {
                let path = format!("blobs/sha256/{}", expected.hex());
                let actual = objects.get(&path).ok_or_else(|| {
                    Error::Integrity(format!("missing archive blob: {}", descriptor.digest))
                })?;
                store.verify_checked(actual, check)?;
                actual.clone()
            }
            Self::Registry(session) => session.blob(descriptor, store, check)?,
        };
        if actual != expected || store.size(&actual)? != descriptor.size {
            return Err(Error::Integrity(format!(
                "descriptor hash/size mismatch: {}",
                descriptor.digest
            )));
        }
        Ok(actual)
    }
}

impl NativeOci {
    pub fn import(
        &self,
        input: ImageInput<'_>,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        platform: GuestPlatform,
        context: Option<&PreparationContext>,
    ) -> Result<NativeImportedImage> {
        let check = || context.map_or(Ok(()), PreparationContext::check);
        check()?;
        fs::create_dir(directory)?;
        let mut auxiliary = BTreeMap::new();
        let mut provenance = BTreeMap::from([("importer".into(), "seedbox-native-oci-v1".into())]);
        let (mut source, document, reference) = match input {
            ImageInput::Registry(reference) => {
                let mut session = self.registry.session(reference)?;
                let document = session.manifest(store, &check)?;
                provenance.insert("registry-reference".into(), reference.into());
                (Source::Registry(Box::new(session)), document, None)
            }
            ImageInput::OciLayout { path, reference } => {
                let root = fs::canonicalize(path).map_err(|error| {
                    Error::Setup(format!(
                        "image layout {}: {error}; supply an existing OCI layout",
                        path.display()
                    ))
                })?;
                check_layout(&read_small(layout_file(&root, "oci-layout")?, &check)?)?;
                let index = retain(layout_file(&root, "index.json")?, store, META_LIMIT, &check)?;
                (Source::Layout(root), index, reference.map(str::to_owned))
            }
            ImageInput::OciArchive { path, reference } => {
                let objects = archive_objects(path, store, &check)?;
                check_layout(
                    &store.read_checked(archive_object(&objects, "oci-layout")?, &check)?,
                )?;
                let index = archive_object(&objects, "index.json")?.clone();
                (
                    Source::Archive(objects),
                    index,
                    reference.map(str::to_owned),
                )
            }
            ImageInput::DockerArchive { path, reference } => {
                let objects = archive_objects(path, store, &check)?;
                auxiliary.insert(
                    "archive-manifest".into(),
                    archive_object(&objects, "manifest.json")?.clone(),
                );
                saved_source(objects, reference, platform, store, &check)?
            }
            ImageInput::Docker { client, reference } | ImageInput::Podman { client, reference } => {
                if reference.is_empty()
                    || reference.starts_with('-')
                    || reference
                        .chars()
                        .any(|character| character.is_whitespace() || character.is_control())
                {
                    return Err(Error::Setup(
                        "local image reference: expected an existing image name or ID without command options".into(),
                    ));
                }
                let archive = directory.join("image.tar");
                let mut command = Command::new(client);
                command
                    .args(["image", "save", "--output"])
                    .arg(&archive)
                    .arg(reference);
                if let Some(context) = context {
                    // Each service imports into its own directory. Hash the complete
                    // path so unsafe/non-UTF-8 names cannot become evidence paths,
                    // and equal basenames in distinct directories stay distinct.
                    let phase = format!(
                        "local-image-save-{}",
                        ContentDigest::of(directory.as_os_str().as_encoded_bytes()).hex()
                    );
                    context.run(&mut command, &phase, None, false, false)?;
                } else {
                    super::checked(&mut command, &directory.join("save.log"))?;
                }
                check()?;
                let objects = archive_objects(&archive, store, &check)?;
                auxiliary.insert(
                    "archive-manifest".into(),
                    archive_object(&objects, "manifest.json")?.clone(),
                );
                provenance.insert("local-image-reference".into(), reference.into());
                saved_source(objects, None, platform, store, &check)?
            }
        };
        let mut manifests = Vec::new();
        select_manifest(
            &document,
            &mut source,
            store,
            platform,
            reference.as_deref(),
            0,
            &check,
            &mut manifests,
        )?;
        if manifests.len() != 1 {
            return Err(Error::Setup(format!(
                "image contains {} matching {} manifests; supply a single image or select an archive reference",
                manifests.len(), platform.as_str()
            )));
        }
        let manifest_digest = manifests.remove(0);
        if document != manifest_digest {
            auxiliary.insert("source-index".into(), document);
        }
        let manifest: Manifest =
            serde_json::from_slice(&store.read_checked(&manifest_digest, &check)?)?;
        if manifest.schema_version != 2
            || ![OCI_CONFIG, DOCKER_CONFIG].contains(&manifest.config.media_type.as_str())
        {
            return Err(Error::Integrity(
                "unsupported image manifest/configuration".into(),
            ));
        }
        let config_digest = source.blob(&manifest.config, store, &check)?;
        let config_bytes = store.read_checked(&config_digest, &check)?;
        let config: ImageConfig = serde_json::from_slice(&config_bytes)?;
        let launch = config_launch(&config, service, platform)?;
        let rootfs: RootfsConfig = serde_json::from_slice(&config_bytes)?;
        if rootfs.rootfs.kind != "layers" || rootfs.rootfs.diff_ids.len() != manifest.layers.len() {
            return Err(Error::Integrity(
                "image rootfs diff_ids do not match layers".into(),
            ));
        }
        let mut filesystem = filesystem::ImageFilesystem::new(&directory.join("files"))?;
        let mut layers = Vec::new();
        for (layer, diff_id) in manifest.layers.iter().zip(rootfs.rootfs.diff_ids) {
            check()?;
            if ![TAR, GZIP, DOCKER_GZIP, ZSTD].contains(&layer.media_type.as_str()) {
                return Err(Error::Integrity(format!(
                    "unsupported layer media type {}",
                    layer.media_type
                )));
            }
            let digest = source.blob(layer, store, &check)?;
            let compressed = store.materialize_checked(
                &digest,
                &directory.join(format!("layer-{}", layers.len())),
                &check,
            )?;
            let file = fs::File::open(&compressed)?;
            let mut decoded: Box<dyn Read> = match layer.media_type.as_str() {
                GZIP | DOCKER_GZIP => Box::new(flate2::read::MultiGzDecoder::new(file)),
                ZSTD => Box::new(zstd::stream::read::Decoder::new(file)?),
                _ => Box::new(file),
            };
            let mut tar = tempfile::NamedTempFile::new_in(directory)?;
            let mut hash = HashWriter {
                output: &mut tar,
                hash: Sha256::new(),
            };
            filesystem::copy_checked(&mut decoded, &mut hash, BLOB_LIMIT, &check)?;
            let actual = ContentDigest::try_from(format!("{:x}", hash.hash.finalize()))?;
            if actual != sha256(&diff_id)? {
                return Err(Error::Integrity(format!(
                    "uncompressed layer hash mismatch: {diff_id}"
                )));
            }
            filesystem.apply_layer(fs::File::open(tar.path())?, &check)?;
            layers.push(digest);
        }
        check()?;
        provenance.insert(
            "resolved-manifest".into(),
            format!("sha256:{}", manifest_digest.hex()),
        );
        Ok(NativeImportedImage {
            image: PreparedImage {
                manifest: manifest_digest,
                config: config_digest,
                layers,
                launch,
            },
            filesystem,
            auxiliary,
            provenance,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn select_manifest(
    digest: &ContentDigest,
    source: &mut Source,
    store: &ContentStore,
    platform: GuestPlatform,
    reference: Option<&str>,
    depth: u32,
    check: &dyn Fn() -> Result<()>,
    selected: &mut Vec<ContentDigest>,
) -> Result<()> {
    check()?;
    if depth > 8 {
        return Err(Error::Integrity(
            "image index nesting exceeds eight levels".into(),
        ));
    }
    let bytes = store.read_checked(digest, check)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("manifests").is_none() {
        if reference.is_some() {
            return Err(Error::Setup("image reference requires an OCI index".into()));
        }
        if value
            .get("schemaVersion")
            .and_then(serde_json::Value::as_u64)
            != Some(2)
            || value
                .get("mediaType")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| ![OCI_MANIFEST, DOCKER_MANIFEST].contains(&kind))
        {
            return Err(Error::Integrity("unsupported image manifest".into()));
        }
        let manifest: Manifest = serde_json::from_value(value)?;
        if ![OCI_CONFIG, DOCKER_CONFIG].contains(&manifest.config.media_type.as_str()) {
            return Err(Error::Integrity(
                "unsupported image configuration media type".into(),
            ));
        }
        let config = source.blob(&manifest.config, store, check)?;
        let config: ImageConfig = serde_json::from_slice(&store.read_checked(&config, check)?)?;
        if config.os == "linux"
            && config.architecture == platform.architecture()
            && !selected.contains(digest)
        {
            selected.push(digest.clone());
        }
        return Ok(());
    }
    let index: Index = serde_json::from_value(value)?;
    if index.schema_version != 2
        || index.manifests.len() > 1000
        || index
            .media_type
            .as_deref()
            .is_some_and(|kind| ![OCI_INDEX, DOCKER_INDEX].contains(&kind))
    {
        return Err(Error::Integrity(
            "unsupported or excessive image index".into(),
        ));
    }
    for entry in index.manifests {
        if reference.is_some_and(|reference| {
            entry
                .annotations
                .get("org.opencontainers.image.ref.name")
                .map(String::as_str)
                != Some(reference)
        }) {
            continue;
        }
        if entry.platform.as_ref().is_some_and(|candidate| {
            candidate.os != "linux"
                || candidate.architecture != platform.architecture()
                || candidate.variant.as_deref().is_some_and(|variant| {
                    !matches!(
                        (platform, variant),
                        (GuestPlatform::LinuxArm64, "v8") | (GuestPlatform::LinuxAmd64, "v1")
                    )
                })
        }) {
            continue;
        }
        if ![OCI_MANIFEST, DOCKER_MANIFEST, OCI_INDEX, DOCKER_INDEX]
            .contains(&entry.descriptor.media_type.as_str())
        {
            return Err(Error::Integrity(
                "unsupported image index descriptor".into(),
            ));
        }
        let child = source.blob(&entry.descriptor, store, check)?;
        select_manifest(
            &child,
            source,
            store,
            platform,
            None,
            depth + 1,
            check,
            selected,
        )?;
        if selected.len() > 1 {
            break;
        }
    }
    Ok(())
}

fn saved_manifest(
    objects: &BTreeMap<String, ContentDigest>,
    reference: Option<&str>,
    platform: GuestPlatform,
    store: &ContentStore,
    check: &dyn Fn() -> Result<()>,
) -> Result<ContentDigest> {
    let images: Vec<SavedImage> = serde_json::from_slice(
        &store.read_checked(archive_object(objects, "manifest.json")?, check)?,
    )?;
    let mut candidates = Vec::new();
    for image in images {
        check()?;
        if reference.is_some_and(|reference| {
            !image
                .repo_tags
                .as_ref()
                .is_some_and(|tags| tags.iter().any(|tag| tag == reference))
        }) {
            continue;
        }
        let config = archive_object(objects, &image.config)?;
        if let Some(filename) = Path::new(&image.config)
            .file_name()
            .and_then(|name| name.to_str())
        {
            if let Some(hex) = filename.strip_suffix(".json").filter(|hex| hex.len() == 64) {
                if config != &ContentDigest::try_from(hex.to_owned())? {
                    return Err(Error::Integrity(
                        "saved image config filename/hash mismatch".into(),
                    ));
                }
            }
        }
        let parsed: ImageConfig = serde_json::from_slice(&store.read_checked(config, check)?)?;
        if parsed.os == "linux" && parsed.architecture == platform.architecture() {
            candidates.push((image, config.clone()));
        }
    }
    if candidates.len() != 1 {
        return Err(Error::Setup(format!("saved image contains {} matching {} images; export one image with the requested tag and platform", candidates.len(), platform.as_str())));
    }
    let (image, config) = candidates.remove(0);
    let config_descriptor = Descriptor {
        media_type: OCI_CONFIG.into(),
        digest: format!("sha256:{}", config.hex()),
        size: store.size(&config)?,
    };
    let layers = image
        .layers
        .iter()
        .map(|path| {
            let digest = archive_object(objects, path)?;
            Ok(Descriptor {
                media_type: TAR.into(),
                digest: format!("sha256:{}", digest.hex()),
                size: store.size(digest)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // Docker-save has no platform manifest. This deterministic OCI envelope
    // names the exact saved config and tar bytes; it is not a registry digest.
    store.put(&serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": OCI_MANIFEST,
        "config": config_descriptor, "layers": layers,
    }))?)
}

fn saved_source(
    objects: BTreeMap<String, ContentDigest>,
    reference: Option<&str>,
    platform: GuestPlatform,
    store: &ContentStore,
    check: &dyn Fn() -> Result<()>,
) -> Result<(Source, ContentDigest, Option<String>)> {
    // New Docker image stores can export an OCI layout plus the legacy manifest
    // compatibility file. Prefer original descriptors, including compression.
    if objects.contains_key("oci-layout") && objects.contains_key("index.json") {
        check_layout(&store.read_checked(archive_object(&objects, "oci-layout")?, check)?)?;
        let index = archive_object(&objects, "index.json")?.clone();
        Ok((
            Source::Archive(objects),
            index,
            reference.map(str::to_owned),
        ))
    } else {
        let manifest = saved_manifest(&objects, reference, platform, store, check)?;
        Ok((Source::saved(objects), manifest, None))
    }
}

fn archive_objects(
    path: &Path,
    store: &ContentStore,
    check: &dyn Fn() -> Result<()>,
) -> Result<BTreeMap<String, ContentDigest>> {
    let file = fs::File::open(path).map_err(|error| {
        Error::Setup(format!(
            "image archive {}: {error}; supply an existing image archive",
            path.display()
        ))
    })?;
    let mut reader = BufReader::new(file);
    let magic = reader.fill_buf()?;
    let input: Box<dyn Read> = if magic.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::MultiGzDecoder::new(reader))
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(zstd::stream::read::Decoder::new(reader)?)
    } else {
        Box::new(reader)
    };
    let input = super::tar_reader::TarReader::new(input, ARCHIVE_LIMIT, check);
    let mut archive = tar::Archive::new(input);
    let mut objects = BTreeMap::new();
    let mut total = 0u64;
    for item in archive.entries()? {
        check()?;
        let entry = item?;
        let name = filesystem::archive_path(&entry.path()?)?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() || name.is_empty() || objects.contains_key(&name)
        {
            return Err(Error::Integrity(format!(
                "duplicate or non-regular image archive object: {name}"
            )));
        }
        total = total
            .checked_add(entry.size())
            .filter(|total| *total <= ARCHIVE_LIMIT)
            .ok_or_else(|| Error::Integrity("image archive exceeds 64 GiB".into()))?;
        if objects.len() >= 1_000_000 {
            return Err(Error::Integrity(
                "image archive exceeds one million objects".into(),
            ));
        }
        let size = entry.size();
        let digest = retain(entry, store, BLOB_LIMIT, check)?;
        if store.size(&digest)? != size {
            return Err(Error::Integrity(format!(
                "truncated image archive object: {name}"
            )));
        }
        objects.insert(name, digest);
    }
    // Drain compression trailers with the same byte/cancellation budget.
    let result = io::copy(&mut archive.into_inner(), &mut io::sink());
    check()?;
    result?;
    Ok(objects)
}

fn archive_object<'a>(
    objects: &'a BTreeMap<String, ContentDigest>,
    name: &str,
) -> Result<&'a ContentDigest> {
    let name = filesystem::archive_path(Path::new(name))?;
    objects
        .get(&name)
        .ok_or_else(|| Error::Integrity(format!("missing image archive object: {name}")))
}

fn layout_file(root: &Path, name: &str) -> Result<fs::File> {
    let name = filesystem::archive_path(Path::new(name))?;
    let mut path = root.to_path_buf();
    let parts: Vec<_> = name.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            Error::Setup(format!(
                "image layout {}: {error}; restore the complete OCI layout",
                path.display()
            ))
        })?;
        if (index + 1 == parts.len() && !metadata.is_file())
            || (index + 1 != parts.len() && !metadata.is_dir())
        {
            return Err(Error::Integrity(format!(
                "image layout object is not a regular file/directory: {}",
                path.display()
            )));
        }
    }
    Ok(fs::File::open(path)?)
}

fn check_layout(bytes: &[u8]) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    if value
        .get("imageLayoutVersion")
        .and_then(serde_json::Value::as_str)
        != Some("1.0.0")
    {
        return Err(Error::Integrity("unsupported OCI layout version".into()));
    }
    Ok(())
}

fn descriptor_digest(descriptor: &Descriptor) -> Result<ContentDigest> {
    sha256(&descriptor.digest)
}

fn descriptor_limit(descriptor: &Descriptor) -> Result<u64> {
    let metadata = [
        OCI_MANIFEST,
        DOCKER_MANIFEST,
        OCI_INDEX,
        DOCKER_INDEX,
        OCI_CONFIG,
        DOCKER_CONFIG,
    ]
    .contains(&descriptor.media_type.as_str());
    let limit = if metadata { META_LIMIT } else { BLOB_LIMIT };
    if descriptor.size > limit {
        return Err(Error::Integrity(if metadata {
            "image metadata exceeds 8 MiB import limit".into()
        } else {
            "image blob exceeds 16 GiB".into()
        }));
    }
    Ok(limit)
}

fn sha256(value: &str) -> Result<ContentDigest> {
    ContentDigest::try_from(
        value
            .strip_prefix("sha256:")
            .ok_or_else(|| Error::Integrity("only sha256 image digests are supported".into()))?
            .to_owned(),
    )
}

fn retain(
    input: impl Read,
    store: &ContentStore,
    limit: u64,
    check: &dyn Fn() -> Result<()>,
) -> Result<ContentDigest> {
    let mut reader = CheckedReader {
        input,
        check,
        remaining: limit,
    };
    let result = store.put_reader_checked(&mut reader, check);
    check()?;
    result
}

fn read_small(input: impl Read, check: &dyn Fn() -> Result<()>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    filesystem::copy_checked(
        &mut input.take(META_LIMIT + 1),
        &mut bytes,
        META_LIMIT,
        check,
    )?;
    Ok(bytes)
}

struct CheckedReader<'a, R> {
    input: R,
    check: &'a dyn Fn() -> Result<()>,
    remaining: u64,
}

impl<R: Read> Read for CheckedReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        (self.check)().map_err(io::Error::other)?;
        let count = self.input.read(buffer)?;
        self.remaining = self
            .remaining
            .checked_sub(count as u64)
            .ok_or_else(|| io::Error::other("image object exceeds import limit"))?;
        Ok(count)
    }
}

struct HashWriter<'a, W> {
    output: &'a mut W,
    hash: Sha256,
}

impl<W: Write> Write for HashWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.output.write(bytes)?;
        self.hash.update(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
