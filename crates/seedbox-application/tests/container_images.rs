//! Image API, export conversion, cancellation and content-cache composition.
use bollard::Docker;
use seedbox_application::{
    config,
    container_images::ContainerTransport,
    execution::CancellationToken,
    guest::Architecture,
    oci::{ImageTransport, RootFilesystem},
    store::{ContentDigest, ContentStore},
    Error,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

struct Engine {
    address: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Engine {
    fn new(cancellation: Option<CancellationToken>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let export = export();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (mut connection, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept image request: {error}"),
                };
                connection.set_nonblocking(false).unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                connection
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(connection.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                recorded.lock().unwrap().push(request.trim().to_owned());
                let body = if request.contains("/version ") {
                    br#"{"ApiVersion":"1.44","Version":"test"}"#.as_slice()
                } else if request.contains("/get?") {
                    if let Some(token) = &cancellation {
                        token.cancel();
                    }
                    &export
                } else {
                    assert!(
                        request.contains("/images/") && request.contains("/json?"),
                        "{request}"
                    );
                    br#"{"Id":"sha256:test","Os":"linux","Architecture":"amd64"}"#.as_slice()
                };
                // Cancellation is allowed to close the connection mid-response.
                let _ = write!(
                    connection,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = connection.write_all(body);
            }
        });
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn exports(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.contains("/get?"))
            .count()
    }
    fn transport(&self, token: CancellationToken, engine: Option<&[u8]>) -> ContainerTransport {
        ContainerTransport {
            docker: Docker::connect_with_http(&self.address, 3, bollard::API_DEFAULT_VERSION)
                .unwrap(),
            cancellation: token,
            progress: |_| {},
            export_cache: engine.map(ContentDigest::of),
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Err(error) = self.worker.take().unwrap().join() {
            if !thread::panicking() {
                std::panic::resume_unwind(error);
            }
        }
    }
}
fn export() -> Vec<u8> {
    let mut layer = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_uid(123);
    header.set_gid(456);
    header.set_mtime(0);
    header.set_size(3);
    layer.append_data(&mut header, "app", &b"app"[..]).unwrap();
    let layer = layer.into_inner().unwrap();
    let mut image = tar::Builder::new(Vec::new());
    for (name, bytes) in [
        (
            "manifest.json",
            br#"[{"Config":"config.json","Layers":["layer.tar"]}]"#.as_slice(),
        ),
        (
            "config.json",
            br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#.as_slice(),
        ),
        ("layer.tar", layer.as_slice()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        image.append_data(&mut header, name, bytes).unwrap();
    }
    image.into_inner().unwrap()
}
fn simulation(directory: &Path) -> config::Simulation {
    config::parse("name: fixture\nservices:\n  app: {image: example/app}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n", &directory.join("compose.yaml"), &BTreeMap::new()).unwrap()
}
#[test]
fn image_export_is_read_only_and_cancellable() {
    for cancel in [false, true] {
        let token = CancellationToken::default();
        let engine = Engine::new(cancel.then(|| token.clone()));
        let directory = tempfile::tempdir().unwrap();
        let simulation = simulation(directory.path());
        let store = ContentStore::new(directory.path().join("store"));
        let result = engine.transport(token, None).import(
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
        assert!(engine
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.starts_with("GET ")));
    }
}
#[test]
fn recorded_exports_are_reused_only_with_their_engine_image_and_present_content() {
    let engine = Engine::new(None);
    let directory = tempfile::tempdir().unwrap();
    let simulation = simulation(directory.path());
    let store = ContentStore::new(directory.path().join("store"));
    let import = |digest: &[u8], name: &str| {
        let imported = engine
            .transport(CancellationToken::default(), Some(digest))
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
    let exported = import(b"engine", "first");
    assert_eq!(engine.exports(), 1);
    assert_eq!(import(b"engine", "second"), exported);
    assert_eq!(engine.exports(), 1);
    assert_eq!(import(b"other engine", "third"), exported);
    assert_eq!(engine.exports(), 2);
    fs::remove_dir_all(directory.path().join("store/sha256")).unwrap();
    assert_eq!(import(b"engine", "fourth"), exported);
    assert_eq!(engine.exports(), 3);
}
