//! Image preparation through the Docker-compatible API. The engine owns builds,
//! caching and layer application; Seedbox retains the exported filesystem.
use crate::{
    config::{ImageSource, Service},
    execution::CancellationToken,
    guest::Architecture,
    oci::{ImageTransport, ImportedImage, RootFilesystem},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use bollard::{
    models::BuildInfoAux,
    query_parameters::{BuildImageOptionsBuilder, CreateImageOptionsBuilder},
    Docker,
};
use futures_util::{Stream, StreamExt};
use std::{collections::BTreeMap, fs, path::Path, time::Duration};
use tokio::io::AsyncWriteExt;

pub struct ContainerTransport {
    pub docker: Docker,
    pub cancellation: CancellationToken,
    pub progress: fn(&str),
    /// Reuse an image's exported filesystem. An image ID names fixed content,
    /// so the same export format yields the same bytes. `false` exports every time.
    pub export_cache: bool,
}

/// Keys cached image exports. Change it whenever export or layer merging
/// changes the bytes produced for the same image.
const EXPORT_FORMAT: &str = "image-export-v2";
fn setup(error: impl std::fmt::Display) -> Error {
    Error::Setup(format!("container image preparation: {error}"))
}
fn immutable_reference(info: bollard::models::ImageInspect) -> Result<String> {
    // Docker's containerd store may return a platform manifest ID that cannot
    // be looked up on its own. A repository digest is an immutable lookup key.
    info.repo_digests
        .unwrap_or_default()
        .into_iter()
        .next()
        .or(info.id)
        .ok_or_else(|| setup("image has no immutable reference"))
}
impl ContainerTransport {
    fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            Err(Error::PreparationCancelled)
        } else {
            Ok(())
        }
    }
    async fn next<S: Stream + Unpin>(&self, stream: &mut S) -> Result<Option<S::Item>> {
        loop {
            self.check()?;
            tokio::select! {
                item = stream.next() => return Ok(item),
                _ = tokio::time::sleep(Duration::from_millis(50)) => (),
            }
        }
    }
    /// Pull or build the service's image; returns the engine client and the
    /// image's immutable reference.
    async fn resolve(
        &self,
        service: &Service,
        directory: &Path,
        architecture: Architecture,
    ) -> Result<(Docker, String)> {
        self.check()?;
        let docker = self.docker.clone().negotiate_version().await.map_err(|error| Error::Setup(format!(
            "cannot connect to a Docker-compatible engine: {error}; start Docker or Podman, or set DOCKER_HOST to its API socket")))?;
        let version = docker.version().await.map_err(setup)?;
        let podman = version
            .components
            .unwrap_or_default()
            .iter()
            .any(|component| component.name == "Podman Engine");
        // Bollard's image helpers do not yet expose Docker's platform query.
        // Select the requested platform explicitly for multi-platform image stores.
        let docker = if podman {
            docker
        } else {
            let platform = format!(
                "%7B%22os%22:%22linux%22,%22architecture%22:%22{}%22%7D",
                architecture.oci_name()
            );
            docker.with_request_modifier(move |mut request| {
                let path = request.uri().path();
                if path.contains("/images/") && (path.ends_with("/json") || path.ends_with("/get"))
                {
                    *request.uri_mut() = format!("{}?platform={platform}", request.uri())
                        .parse()
                        .expect("valid image URI with percent-encoded platform");
                }
                request
            })
        };
        let image = match &service.source {
            ImageSource::Registry { reference } => match docker.inspect_image(reference).await {
                Ok(info)
                    if info.os.as_deref() == Some("linux")
                        && info.architecture.as_deref() == Some(architecture.oci_name()) =>
                {
                    (self.progress)(&format!("using local image {reference}"));
                    immutable_reference(info)?
                }
                // Docker's containerd store can return an incomplete inspect
                // record for a platform that has not been pulled yet.
                Ok(_)
                | Err(bollard::errors::Error::JsonDataError { .. })
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {
                    (self.progress)(&format!(
                        "pulling {reference} for {}",
                        architecture.platform()
                    ));
                    let mut pull = docker.create_image(
                        Some(
                            CreateImageOptionsBuilder::default()
                                .from_image(reference)
                                .platform(architecture.platform())
                                .build(),
                        ),
                        None,
                        None,
                    );
                    while let Some(event) = self.next(&mut pull).await? {
                        let event = event.map_err(setup)?;
                        if let Some(error) = event.error_detail {
                            return Err(setup(
                                error.message.unwrap_or_else(|| "image pull failed".into()),
                            ));
                        }
                        if let Some(status) = event.status {
                            (self.progress)(&status);
                        }
                    }
                    immutable_reference(docker.inspect_image(reference).await.map_err(setup)?)?
                }
                Err(error) => return Err(setup(error)),
            },
            ImageSource::Build {
                context,
                dockerfile,
            } => {
                (self.progress)(&format!(
                    "building {} (engine cache enabled)",
                    dockerfile.display()
                ));
                let tar = directory.join("context.tar");
                crate::build_context::archive(context, dockerfile, &tar, &|| self.check())?;
                let relative = dockerfile
                    .strip_prefix(context)
                    .map_err(|_| setup("Dockerfile must be inside its build context"))?;
                let file = tokio::fs::File::open(&tar).await?;
                let mut options = BuildImageOptionsBuilder::default()
                    .dockerfile(&relative.to_string_lossy())
                    .platform(architecture.platform())
                    .rm(true)
                    .forcerm(true)
                    .nocache(false);
                let session = tempfile::Builder::new()
                    .prefix("seedbox-build-")
                    .tempdir()?;
                if !podman {
                    options = options
                        .version(bollard::query_parameters::BuilderVersion::BuilderBuildKit)
                        .session(
                            &session
                                .path()
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy(),
                        );
                }
                let options = options.build();
                let mut build = docker.build_image(
                    options,
                    None,
                    Some(bollard::body_try_stream(tokio_util::io::ReaderStream::new(
                        file,
                    ))),
                );
                let mut id = None;
                while let Some(event) = self.next(&mut build).await? {
                    let event = event.map_err(setup)?;
                    if let Some(error) = event.error_detail {
                        return Err(setup(
                            error.message.unwrap_or_else(|| "image build failed".into()),
                        ));
                    }
                    match event.aux {
                        Some(BuildInfoAux::Default(aux)) => id = aux.id,
                        Some(BuildInfoAux::BuildKit(status)) => {
                            for vertex in status.vertexes {
                                if !vertex.name.is_empty() {
                                    (self.progress)(&format!(
                                        "{}{}",
                                        if vertex.cached { "cached: " } else { "" },
                                        vertex.name
                                    ));
                                }
                                if !vertex.error.is_empty() {
                                    return Err(setup(vertex.error));
                                }
                            }
                            for log in status.logs {
                                for line in String::from_utf8_lossy(&log.msg).lines() {
                                    (self.progress)(line);
                                }
                            }
                        }
                        None => (),
                    }
                    if let Some(stream) = event.stream {
                        for line in stream.lines().filter(|line| !line.is_empty()) {
                            (self.progress)(line);
                        }
                    }
                }
                id.ok_or_else(|| setup("engine build completed without an image ID"))?
            }
        };
        self.check()?;
        let info = docker.inspect_image(&image).await.map_err(setup)?;
        if info.os.as_deref() != Some("linux")
            || info.architecture.as_deref() != Some(architecture.oci_name())
        {
            return Err(setup(format!("image must be {}", architecture.platform())));
        }
        Ok((docker, image))
    }
    async fn export(
        &self,
        docker: &Docker,
        image: &str,
        directory: &Path,
    ) -> Result<serde_json::Value> {
        self.check()?;
        (self.progress)("exporting image layers");
        let mut stream = docker.export_image(image);
        let archive = directory.join("image.tar");
        let mut file = tokio::fs::File::create(&archive).await?;
        let mut size = 0u64;
        while let Some(bytes) = self.next(&mut stream).await? {
            let bytes = bytes.map_err(setup)?;
            size = size
                .checked_add(bytes.len() as u64)
                .filter(|n| *n <= 4 << 30)
                .ok_or_else(|| setup("exported image exceeds 4 GiB"))?;
            file.write_all(&bytes).await?;
        }
        file.flush().await?;
        drop(file);
        (self.progress)("merging image layers");
        crate::image_archive::merge(
            &archive,
            &directory.join("layers"),
            &directory.join("filesystem.tar"),
            &self.cancellation,
        )
    }
}
impl ImageTransport for ContainerTransport {
    fn import(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context: Option<(&crate::preparation::PreparationContext, &str)>,
        architecture: Architecture,
    ) -> Result<ImportedImage> {
        self.check()?;
        if let Some((context, _)) = context {
            context.check()?;
        }
        fs::create_dir_all(directory)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(setup)?;
        let (docker, image) = runtime.block_on(self.resolve(service, directory, architecture))?;
        let record = self.export_cache.then(|| {
            ContentDigest::of(
                serde_json::json!([EXPORT_FORMAT, architecture.platform(), image])
                    .to_string()
                    .as_bytes(),
            )
        });
        let raw = directory.join("metadata");
        fs::create_dir(&raw)?;
        let (config_bytes, layer, rootfs) = match record
            .as_ref()
            .and_then(|record| cached_export(store, record))
        {
            Some((config_bytes, layer)) => {
                (self.progress)("reusing exported image layers");
                let rootfs = store.materialize(&layer, &raw)?;
                (config_bytes, layer, rootfs)
            }
            None => {
                let mut config = runtime.block_on(self.export(&docker, &image, directory))?;
                let rootfs = directory.join("filesystem.tar");
                let layer = store.put_file(&rootfs)?;
                // This is an OCI-shaped snapshot of the engine's resolved filesystem,
                // not a claim to be the registry's original layered manifest.
                config["rootfs"] = serde_json::json!({"type":"layers", "diff_ids":[format!("sha256:{}", layer.hex())]});
                let config_bytes = serde_json::to_vec(&config)?;
                let config_digest = store.put(&config_bytes)?;
                if let Some(record) = &record {
                    store.set_record(record, &config_digest)?;
                }
                // The exported filesystem is scratch in this directory; link it rather
                // than copying it back out of the store.
                let linked = raw.join(layer.hex());
                fs::hard_link(&rootfs, &linked)?;
                (config_bytes, layer, linked)
            }
        };
        let config_digest = store.put(&config_bytes)?;
        store.materialize(&config_digest, &raw)?;
        fs::write(
            raw.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion":2,
                "config":{"mediaType":"application/vnd.oci.image.config.v1+json", "digest":format!("sha256:{}",config_digest.hex()), "size":config_bytes.len()},
                "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar", "digest":format!("sha256:{}",layer.hex()), "size":fs::metadata(&rootfs)?.len()}]
            }))?,
        )?;
        let image = crate::oci::import_metadata_for_arch(&raw, service, store, architecture)?;
        let archive = directory.join("rootfs.tar");
        prefix_archive(&rootfs, &archive, &|| self.check())?;
        Ok(ImportedImage {
            image,
            rootfs: RootFilesystem::Archive(archive),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        })
    }
}

/// The config bytes and filesystem object a recorded export points at, when both
/// are still in the store. Anything else is a miss and exports again.
fn cached_export(store: &ContentStore, record: &ContentDigest) -> Option<(Vec<u8>, ContentDigest)> {
    let config = store.read(&store.record(record)?).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&config).ok()?;
    let [layer] = value["rootfs"]["diff_ids"].as_array()?.as_slice() else {
        return None;
    };
    let layer =
        ContentDigest::try_from(layer.as_str()?.strip_prefix("sha256:")?.to_owned()).ok()?;
    store.size(&layer).ok()?;
    Some((config, layer))
}

/// Prefix metadata only. Guest filesystem paths are never extracted on the host.
fn prefix_archive(input: &Path, output: &Path, check: &dyn Fn() -> Result<()>) -> Result<()> {
    fn relative(path: &Path) -> Result<std::path::PathBuf> {
        let mut result = std::path::PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(part) => result.push(part),
                std::path::Component::CurDir => (),
                _ => return Err(setup("unsafe path in exported filesystem")),
            }
        }
        Ok(result)
    }
    let mut root = tar::Header::new_gnu();
    root.set_entry_type(tar::EntryType::Directory);
    root.set_mode(0o755);
    root.set_uid(0);
    root.set_gid(0);
    root.set_mtime(0);
    root.set_size(0);
    let mut scan = tar::Archive::new(std::io::BufReader::new(fs::File::open(input)?));
    let mut found_root = false;
    for entry in scan.entries_with_seek()? {
        check()?;
        let mut entry = entry?;
        if relative(&entry.path()?)?.as_os_str().is_empty() {
            if found_root || !entry.header().entry_type().is_dir() {
                return Err(setup("invalid or duplicate export root"));
            }
            root = export_header(&mut entry, Path::new("."))?;
            found_root = true;
        }
    }
    let mut result = tar::Builder::new(std::io::BufWriter::new(fs::File::create(output)?));
    result.append_data(&mut root, "rootfs", std::io::empty())?;
    let mut input = tar::Archive::new(std::io::BufReader::new(fs::File::open(input)?));
    for entry in input.entries()? {
        check()?;
        let mut entry = entry?;
        let path = relative(&entry.path()?)?;
        if path.as_os_str().is_empty() {
            continue;
        }
        let mut header = export_header(&mut entry, &path)?;
        let name = Path::new("rootfs").join(path);
        if let Some(link) = entry.link_name()?.map(|path| path.into_owned()) {
            let target = if header.entry_type().is_hard_link() {
                Path::new("rootfs").join(relative(&link)?)
            } else {
                link
            };
            result.append_link(&mut header, name, target)?;
        } else {
            result.append_data(&mut header, name, &mut entry)?;
        }
    }
    std::io::Write::flush(&mut result.into_inner()?)?;
    Ok(())
}

pub(crate) fn export_header<R: std::io::Read>(
    entry: &mut tar::Entry<'_, R>,
    path: &Path,
) -> Result<tar::Header> {
    let mut header = entry.header().clone();
    header.set_size(entry.size());
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            let field = extension.key().map_err(setup)?;
            match field {
                "uid" | "gid" | "mtime" => {
                    let value = extension.value().map_err(setup)?;
                    // Newc stores whole seconds. Fractional timestamps have
                    // the same truncation as the existing filesystem writer.
                    let value = if field == "mtime" {
                        if let Some((whole, fraction)) = value.split_once('.') {
                            if !fraction.bytes().all(|b| b.is_ascii_digit()) {
                                return Err(setup("invalid export timestamp"));
                            }
                            whole
                        } else {
                            value
                        }
                    } else {
                        value
                    };
                    let value = value.parse::<u32>().map_err(setup)? as u64;
                    match field {
                        "uid" => header.set_uid(value),
                        "gid" => header.set_gid(value),
                        _ => header.set_mtime(value),
                    }
                }
                "path" | "linkpath" | "size" | "atime" | "ctime" | "uname" | "gname" => (),
                // Overlay filesystems record their own bookkeeping (origin, redirect,
                // opaque directories) in these xattrs, and rootless engines export
                // them. They describe the build host's layer storage, not the image.
                _ if field.starts_with("SCHILY.xattr.user.overlay.")
                    || field.starts_with("SCHILY.xattr.trusted.overlay.") => {}
                _ => {
                    return Err(setup(format!(
                        "exported filesystem {}: unsupported extended metadata {field}",
                        path.display()
                    )))
                }
            }
        }
    }
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    #[test]
    fn overlay_bookkeeping_xattrs_are_ignored() {
        for key in [
            "SCHILY.xattr.user.overlay.origin",
            "SCHILY.xattr.trusted.overlay.opaque",
        ] {
            let mut tar = tar::Builder::new(Vec::new());
            tar.append_pax_extensions([(key, &b"y"[..])]).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(0);
            tar.append_data(&mut header, "bin/app", std::io::empty())
                .unwrap();
            let bytes = tar.into_inner().unwrap();
            let mut archive = tar::Archive::new(bytes.as_slice());
            let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
            assert!(
                export_header(&mut entry, Path::new("bin/app")).is_ok(),
                "{key}"
            );
        }
    }
    #[test]
    fn unsupported_xattr_names_the_image_path_and_key() {
        let mut tar = tar::Builder::new(Vec::new());
        tar.append_pax_extensions([("SCHILY.xattr.security.capability", &b"value"[..])])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(0);
        tar.append_data(&mut header, "bin/app", std::io::empty())
            .unwrap();
        let bytes = tar.into_inner().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
        let error = export_header(&mut entry, Path::new("bin/app"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("bin/app"), "{error}");
        assert!(
            error.contains("SCHILY.xattr.security.capability"),
            "{error}"
        );
    }

    #[test]
    fn image_export_is_read_only_and_cancellable() {
        for cancel in [false, true] {
            let token = CancellationToken::default();
            let server_token = token.clone();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_uid(123);
            header.set_gid(456);
            header.set_mtime(0);
            header.set_size(3);
            tar.append_data(&mut header, "app", &b"app"[..]).unwrap();
            let layer = tar.into_inner().unwrap();
            let config = br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#;
            let manifest = br#"[{"Config":"config.json","Layers":["layer.tar"]}]"#;
            let mut image = tar::Builder::new(Vec::new());
            for (name, bytes) in [
                ("manifest.json", &manifest[..]),
                ("config.json", &config[..]),
                ("layer.tar", &layer[..]),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o644);
                header.set_size(bytes.len() as u64);
                image.append_data(&mut header, name, bytes).unwrap();
            }
            let filesystem = image.into_inner().unwrap();
            let server = std::thread::spawn(move || {
                let mut requests = Vec::new();
                loop {
                    let (mut connection, _) = listener.accept().unwrap();
                    connection
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut reader = std::io::BufReader::new(connection.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let request = line.trim().to_owned();
                    let mut length = 0;
                    loop {
                        line.clear();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    reader.read_exact(&mut vec![0; length]).unwrap();
                    let body = if request.contains("/version ") {
                        br#"{"ApiVersion":"1.44","Version":"test"}"#.to_vec()
                    } else if request.contains("/get?") {
                        if cancel {
                            server_token.cancel();
                        }
                        filesystem.clone()
                    } else {
                        assert!(
                            request.contains("/images/") && request.contains("/json?"),
                            "{request}"
                        );
                        br#"{"Id":"sha256:test","Os":"linux","Architecture":"amd64"}"#.to_vec()
                    };
                    let done = request.contains("/get?");
                    requests.push(request);
                    let status = "200 OK";
                    let _ = write!(
                        connection,
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = connection.write_all(&body);
                    if done {
                        return requests;
                    }
                }
            });
            let directory = tempfile::tempdir().unwrap();
            let simulation = crate::config::parse("name: fixture\nservices:\n  app: {image: example/app}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n", &directory.path().join("compose.yaml"), &BTreeMap::new()).unwrap();
            let transport = ContainerTransport {
                docker: Docker::connect_with_http(
                    &format!("http://{address}"),
                    10,
                    bollard::API_DEFAULT_VERSION,
                )
                .unwrap(),
                cancellation: token,
                progress: |_| {},
                export_cache: false,
            };
            let store = ContentStore::new(directory.path().join("store"));
            let result = transport.import(
                &simulation.services["app"],
                &directory.path().join("image"),
                &store,
                None,
                Architecture::Amd64,
            );
            if cancel {
                assert!(matches!(result, Err(Error::PreparationCancelled)));
            } else {
                let imported = result.unwrap();
                assert_eq!(imported.image.launch.argv, ["/app"]);
                let RootFilesystem::Archive(path) = imported.rootfs else {
                    panic!("expected archive")
                };
                let mut archive = tar::Archive::new(fs::File::open(path).unwrap());
                let entry = archive.entries().unwrap().nth(1).unwrap().unwrap();
                assert_eq!(entry.path().unwrap().as_ref(), Path::new("rootfs/app"));
                assert_eq!(
                    (entry.header().uid().unwrap(), entry.header().gid().unwrap()),
                    (123, 456)
                );
            }
            let requests = server.join().unwrap();
            assert!(requests.iter().all(|request| request.starts_with("GET ")));
        }
    }

    #[test]
    fn a_recorded_export_is_reused_for_the_same_engine_and_image() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_mtime(0);
        header.set_size(3);
        tar.append_data(&mut header, "app", &b"app"[..]).unwrap();
        let layer = tar.into_inner().unwrap();
        let config = br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#;
        let manifest = br#"[{"Config":"config.json","Layers":["layer.tar"]}]"#;
        let mut image = tar::Builder::new(Vec::new());
        for (name, bytes) in [
            ("manifest.json", &manifest[..]),
            ("config.json", &config[..]),
            ("layer.tar", &layer[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(bytes.len() as u64);
            image.append_data(&mut header, name, bytes).unwrap();
        }
        let export = image.into_inner().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let exports = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served = exports.clone();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let mut connection = connection.unwrap();
                let mut reader = std::io::BufReader::new(connection.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                    line.clear();
                }
                let body = if request.contains("/version ") {
                    br#"{"ApiVersion":"1.44","Version":"test"}"#.to_vec()
                } else if request.contains("/get?") {
                    served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    export.clone()
                } else {
                    br#"{"Id":"sha256:test","Os":"linux","Architecture":"amd64"}"#.to_vec()
                };
                let _ = write!(
                    connection,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = connection.write_all(&body);
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let simulation = crate::config::parse("name: fixture\nservices:\n  app: {image: example/app}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n", &directory.path().join("compose.yaml"), &BTreeMap::new()).unwrap();
        let store = ContentStore::new(directory.path().join("store"));
        let import = |cache: bool, name: &str| {
            let transport = ContainerTransport {
                docker: Docker::connect_with_http(
                    &format!("http://{address}"),
                    10,
                    bollard::API_DEFAULT_VERSION,
                )
                .unwrap(),
                cancellation: CancellationToken::default(),
                progress: |_| {},
                export_cache: cache,
            };
            let imported = transport
                .import(
                    &simulation.services["app"],
                    &directory.path().join(name),
                    &store,
                    None,
                    Architecture::Amd64,
                )
                .unwrap();
            let RootFilesystem::Archive(path) = &imported.rootfs else {
                panic!("expected archive")
            };
            (imported.image, fs::read(path).unwrap())
        };
        let exported = import(true, "first");
        assert_eq!(exports.load(std::sync::atomic::Ordering::SeqCst), 1);
        let reused = import(true, "second");
        assert_eq!(exports.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reused, exported);
        // Without the cache every import exports again.
        assert_eq!(import(false, "third"), exported);
        assert_eq!(exports.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A record whose objects are gone is a miss, not an error.
        fs::remove_dir_all(directory.path().join("store/sha256")).unwrap();
        assert_eq!(import(true, "fourth"), exported);
        assert_eq!(exports.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
