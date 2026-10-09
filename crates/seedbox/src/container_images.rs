//! Image preparation through the Docker-compatible API. The engine owns builds,
//! caching and layer application; Seedbox retains the exported filesystem.
use crate::{
    config::{ImageSource, Service},
    execution::CancellationToken,
    guest::Architecture,
    oci::{ImageTransport, ImportedImage, RootFilesystem},
    store::ContentStore,
    Error, Result,
};
use bollard::{
    models::{BuildInfoAux, ContainerCreateBody},
    query_parameters::{
        BuildImageOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        RemoveContainerOptionsBuilder,
    },
    Docker,
};
use futures_util::{Stream, StreamExt};
use std::{collections::BTreeMap, fs, path::Path, time::Duration};
use tokio::io::AsyncWriteExt;

pub struct ContainerTransport {
    pub docker: Docker,
    pub cancellation: CancellationToken,
    pub progress: fn(&str),
}
fn setup(error: impl std::fmt::Display) -> Error {
    Error::Setup(format!("container image preparation: {error}"))
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
    async fn prepare(
        &self,
        service: &Service,
        directory: &Path,
        architecture: Architecture,
    ) -> Result<serde_json::Value> {
        self.check()?;
        let docker = self.docker.clone().negotiate_version().await.map_err(|error| Error::Setup(format!(
            "cannot connect to a Docker-compatible engine: {error}; start Docker or Podman, or set DOCKER_HOST to its API socket")))?;
        let version = docker.version().await.map_err(setup)?;
        let podman = version
            .components
            .unwrap_or_default()
            .iter()
            .any(|component| component.name == "Podman Engine");
        let image = match &service.source {
            ImageSource::Registry { reference } => match docker.inspect_image(reference).await {
                Ok(info)
                    if info.os.as_deref() == Some("linux")
                        && info.architecture.as_deref() == Some(architecture.oci_name()) =>
                {
                    (self.progress)(&format!("using local image {reference}"));
                    info.id.ok_or_else(|| setup("image has no immutable ID"))?
                }
                Ok(_)
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
                    docker
                        .inspect_image(reference)
                        .await
                        .map_err(setup)?
                        .id
                        .ok_or_else(|| setup("pulled image has no immutable ID"))?
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
        (self.progress)("exporting image filesystem");
        // Never start this container. It only gives the engine an owned, immutable
        // image filesystem to export. Remove its anonymous volumes on every exit.
        let container = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .platform(architecture.platform())
                        .build(),
                ),
                ContainerCreateBody {
                    image: Some(image),
                    entrypoint: Some(vec!["/seedbox-export-only".into()]),
                    ..Default::default()
                },
            )
            .await
            .map_err(setup)?;
        let export = async {
            let mut stream = docker.export_container(&container.id);
            let mut file = tokio::fs::File::create(directory.join("filesystem.tar")).await?;
            let mut size = 0u64;
            while let Some(bytes) = self.next(&mut stream).await? {
                let bytes = bytes.map_err(setup)?;
                size = size
                    .checked_add(bytes.len() as u64)
                    .filter(|size| *size <= 4 << 30)
                    .ok_or_else(|| setup("exported filesystem exceeds 4 GiB"))?;
                file.write_all(&bytes).await?;
            }
            file.flush().await?;
            Ok::<_, Error>(())
        }
        .await;
        let cleanup = docker
            .remove_container(
                &container.id,
                Some(RemoveContainerOptionsBuilder::default().v(true).build()),
            )
            .await;
        if let Err(error) = cleanup {
            return Err(setup(format!("{previous}could not remove owned export container {}: {error}; remove it with your container engine", container.id,
                previous=export.err().map(|error| format!("{error}; ")).unwrap_or_default())));
        }
        export?;
        Ok(
            serde_json::json!({"os": info.os, "architecture": info.architecture, "config": info.config}),
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
        let mut config = runtime.block_on(self.prepare(service, directory, architecture))?;
        let rootfs = directory.join("filesystem.tar");
        let layer = store.put_file(&rootfs)?;
        // This is an OCI-shaped snapshot of the engine's resolved filesystem,
        // not a claim to be the registry's original layered manifest.
        config["rootfs"] =
            serde_json::json!({"type":"layers", "diff_ids":[format!("sha256:{}", layer.hex())]});
        let config_bytes = serde_json::to_vec(&config)?;
        let config_digest = store.put(&config_bytes)?;
        let raw = directory.join("metadata");
        fs::create_dir(&raw)?;
        store.materialize(&layer, &raw)?;
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
    let mut input = tar::Archive::new(fs::File::open(input)?);
    let mut entries = input.entries()?;
    let first = entries.next().transpose()?;
    let first_is_root = first
        .as_ref()
        .map(|entry| relative(&entry.path()?))
        .transpose()?
        .is_some_and(|path| path.as_os_str().is_empty());
    let mut root = tar::Header::new_gnu();
    root.set_entry_type(tar::EntryType::Directory);
    root.set_mode(0o755);
    root.set_uid(0);
    root.set_gid(0);
    root.set_mtime(0);
    root.set_size(0);
    if first_is_root {
        root = first
            .as_ref()
            .ok_or_else(|| setup("missing export root"))?
            .header()
            .clone();
        if !root.entry_type().is_dir() {
            return Err(setup("export root is not a directory"));
        }
    }
    let mut result = tar::Builder::new(fs::File::create(output)?);
    result.append_data(&mut root, "rootfs", std::io::empty())?;
    for entry in first
        .into_iter()
        .filter(|_| !first_is_root)
        .map(Ok)
        .chain(entries)
    {
        check()?;
        let mut entry = entry?;
        let path = relative(&entry.path()?)?;
        if path.as_os_str().is_empty() {
            return Err(setup("duplicate export root"));
        }
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
                    _ => {
                        return Err(setup(
                            "exported filesystem contains unsupported extended metadata",
                        ))
                    }
                }
            }
        }
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
    result.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    #[test]
    fn exported_files_are_retained_and_owned_container_is_removed_even_on_cancel() {
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
            let filesystem = tar.into_inner().unwrap();
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
                    } else if request.contains("/images/") {
                        br#"{"Id":"sha256:test","Os":"linux","Architecture":"amd64","Config":{"Cmd":["/app"],"User":"","WorkingDir":""}}"#.to_vec()
                    } else if request.contains("/containers/create") {
                        br#"{"Id":"owned-export","Warnings":[]}"#.to_vec()
                    } else if request.contains("/export ") {
                        if cancel {
                            server_token.cancel();
                        }
                        filesystem.clone()
                    } else {
                        assert!(
                            request.starts_with("DELETE ")
                                && request.contains("owned-export")
                                && request.contains("v=true"),
                            "{request}"
                        );
                        Vec::new()
                    };
                    let done = request.starts_with("DELETE ");
                    requests.push(request);
                    let status = if done { "204 No Content" } else { "200 OK" };
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
            assert!(!requests.iter().any(|request| request.contains("/start")));
        }
    }
}
