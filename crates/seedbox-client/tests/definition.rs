//! Definition submission over a private, bounded HTTP peer.
use seedbox_client::Client;
use std::time::Duration;
struct Server(Option<std::thread::JoinHandle<Vec<String>>>);
impl Server {
    fn join(mut self) -> std::thread::Result<Vec<String>> {
        self.0.take().unwrap().join()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}
fn timeline_resource(created: u64) -> seedbox_api::Simulation {
    let spec = serde_json::from_value(serde_json::json!({
        "seed":42,"oracle":{"image":"example/oracle:latest"}
    }))
    .unwrap();
    let mut resource = seedbox_api::Simulation::queued(
        seedbox_api::Metadata {
            name: "test".into(),
            labels: Default::default(),
            annotations: Default::default(),
            id: "k7m2p9qx".parse().unwrap(),
            revision: 1,
            created_at_unix_ms: created,
            deletion_requested_at_unix_ms: None,
            parent: None,
            checkpoint: None,
        },
        spec,
    );
    resource.status.phase = seedbox_api::Phase::Completed;
    resource
}

fn timeline_server(responses: Vec<seedbox_api::Simulation>) -> (Client, Server) {
    use std::io::{Read, Write};
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let path = temporary.path().join("api.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = Client::connect_to(&path).unwrap();
    let worker = std::thread::spawn(move || {
        let _temporary = temporary;
        let mut requests = Vec::new();
        for response in responses {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "expected timeline HTTP request"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let body_offset = loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break offset + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..body_offset]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < body_offset + length {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            requests.push(String::from_utf8(bytes).unwrap());
            let body = serde_json::to_vec(&response).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
        std::fs::remove_file(path).unwrap();
        requests
    });
    (client, Server(Some(worker)))
}

#[tokio::test]
async fn full_definition_submission_needs_one_post_and_preserves_metadata_and_options() {
    let mut resource = timeline_resource(10);
    let asset = format!("sha256:{}", "a".repeat(64));
    resource.spec = serde_json::from_value(serde_json::json!({
            "architecture":"aarch64", "seed":7, "clock":{"epochSeconds":86400},
            "runtime":{"kernel":asset,"engine":asset,"execution":{}},
            "machines":{}, "oracle":{"bootArchive":asset,"memoryBytes":1073741824_u64,
                "process":{"argv":["/test"],"environment":{},"workingDirectory":"/","user":"0"},"volumes":{}},
            "network":{"name":"default","latency_ns":1000000,"jitter_ns":0},
            "simulator":{"blocksPerProgressCall":17,"schedule":{"timer_jitter_ns":0,"stop_tolerance_ns":0},
                "disk":{"torn_writes":false},"oracleControl":false},
            "start":{"kind":"fresh"},
            "overrides":[{"id":"external","atNs":42,"action":{"action":"future"}}]
        })).unwrap();
    resource.metadata.name = "saved experiment".into();
    resource
        .metadata
        .labels
        .insert("application".into(), "demo".into());
    resource.options.wall_time_ns = Some(9999);
    let document = serde_json::to_value(&resource).unwrap();
    let (client, worker) = timeline_server(vec![resource]);
    client.submit_definition(&document, true).await.unwrap();
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /v1/simulations "));
    let body = requests[0].split_once("\r\n\r\n").unwrap().1;
    let request: seedbox_api::SubmitRequest = serde_json::from_str(body).unwrap();
    assert_eq!(request.metadata.name, "saved experiment");
    assert_eq!(request.metadata.labels["application"], "demo");
    assert!(request.options.start_paused);
    assert_eq!(request.options.wall_time_ns, Some(9999));
    assert_eq!(request.spec.program[0].key, "external");
    assert!(matches!(
        request.spec.inputs,
        seedbox_api::InputSource::Resolved { .. }
    ));
    assert!(!request.request_id.is_empty());
}
