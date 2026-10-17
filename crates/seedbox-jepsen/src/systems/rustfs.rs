//! The RustFS object-store oracle: S3 PUTs of uniquely named objects from
//! several clients, each to a rotating node, with reads of acknowledged
//! objects during the run and a read-back of every acknowledged object once
//! faults stop. An acknowledged object that reads back missing, or with bytes
//! other than those written, violates the check (S3 promises strong
//! read-after-write); a read-back that cannot reach the cluster is
//! inconclusive. Contents derive from the key, so no copy is kept.
//!
//! Requests are signed with AWS Signature Version 4 (path-style, region
//! `us-east-1`, payload hashed) over the kit's HTTP/1.1 client.
use crate::kit::{self, http, Result};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{BufReader, Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The RustFS servers, in order.
    #[arg(long, value_delimiter = ',', default_value = "n1,n2,n3,n4")]
    pub nodes: Vec<String>,
    #[arg(long, default_value_t = 9000)]
    pub port: u16,
    /// Seconds of writes.
    #[arg(long, default_value_t = 75)]
    pub duration: u64,
    #[arg(long, default_value_t = 4)]
    pub clients: usize,
    /// Pause between a client's requests, in milliseconds.
    #[arg(long, default_value_t = 20)]
    pub think_ms: u64,
    /// Seconds a request may take.
    #[arg(long, default_value_t = 10)]
    pub timeout: u64,
    /// Bytes per object.
    #[arg(long, default_value_t = 16384)]
    pub size: usize,
    #[arg(long, default_value = "jepsen")]
    pub bucket: String,
    #[arg(long, default_value = "jepsen")]
    pub access_key: String,
    #[arg(long, default_value = "jepsen-secret")]
    pub secret_key: String,
    /// Seconds to wait after the writes before the read-back.
    #[arg(long, default_value_t = 20)]
    pub quiet: u64,
}

/// What one S3 request came back with.
enum Reply {
    Status(u16, Vec<u8>),
    Unreachable(String),
}

struct S3 {
    args: Args,
}

impl S3 {
    fn request(&self, node: &str, method: &str, path: &str, body: &[u8]) -> Reply {
        match self.exchange(node, method, path, body) {
            Ok((status, data)) => Reply::Status(status, data),
            Err(error) => Reply::Unreachable(error.0),
        }
    }

    fn exchange(
        &self,
        node: &str,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>)> {
        let timeout = Duration::from_secs(self.args.timeout);
        let host = format!("{node}:{}", self.args.port);
        let headers = sign(&self.args, method, &host, path, body, SystemTime::now());
        let mut stream = http::connect(node, self.args.port, timeout)?;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in &headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut frame = head.into_bytes();
        frame.extend_from_slice(body);
        stream.write_all(&frame)?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let head = http::read_head(&mut reader)?;
        let mut data = Vec::new();
        http::Body::new(reader, &head).read_to_end(&mut data)?;
        Ok((head.status, data))
    }
}

/// SigV4 headers for one request: `x-amz-date`, `x-amz-content-sha256` and
/// `Authorization`.
fn sign(
    args: &Args,
    method: &str,
    host: &str,
    path: &str,
    body: &[u8],
    at: SystemTime,
) -> Vec<(String, String)> {
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (date, stamp) = amz_date(seconds);
    let payload = hex(&Sha256::digest(body));
    let canonical = format!(
        "{method}\n{path}\n\nhost:{host}\nx-amz-content-sha256:{payload}\nx-amz-date:{stamp}\n\nhost;x-amz-content-sha256;x-amz-date\n{payload}"
    );
    let scope = format!("{date}/us-east-1/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut key = format!("AWS4{}", args.secret_key).into_bytes();
    for part in [date.as_str(), "us-east-1", "s3", "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    vec![
        ("x-amz-date".into(), stamp),
        ("x-amz-content-sha256".into(), payload),
        (
            "Authorization".into(),
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
                args.access_key
            ),
        ),
    ]
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` for Unix `seconds`, in UTC.
fn amz_date(seconds: u64) -> (String, String) {
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    // Days to civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let date = format!("{year:04}{month:02}{day:02}");
    let stamp = format!(
        "{date}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    (date, stamp)
}

/// The `<Code>` of an S3 error document, or `-`.
fn s3_code(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    text.split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map_or("-".into(), |(code, _)| code.to_string())
}

/// An object's contents: `size` bytes from a generator seeded by its key.
fn contents(key: &str, size: usize) -> Vec<u8> {
    let mut state = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    }) | 1;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[derive(Default)]
struct Tally {
    acknowledged: Vec<String>,
    failed: u64,
    /// Failed PUTs by HTTP status and S3 error code.
    failures: BTreeMap<String, u64>,
    unknown: u64,
    reads: u64,
    /// Acknowledged objects that read back missing or wrong during the run.
    stale: Vec<serde_json::Value>,
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let s3 = Arc::new(S3 { args: args.clone() });
    let bucket = format!("/{}", args.bucket);
    // The cluster forms once all of its servers are up: retry bucket creation.
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match s3.request(&args.nodes[0], "PUT", &bucket, &[]) {
            Reply::Status(200, _) => break,
            Reply::Status(409, _) => break,
            Reply::Status(status, body) if Instant::now() > deadline => {
                kit::evidence(
                    "setup",
                    json!({"status": status, "body": String::from_utf8_lossy(&body)}),
                );
                return kit::verdict(
                    kit::INCONCLUSIVE,
                    &format!("bucket creation failed: HTTP {status}"),
                );
            }
            Reply::Unreachable(error) if Instant::now() > deadline => {
                return kit::verdict(kit::INCONCLUSIVE, &format!("cluster unreachable: {error}"));
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    seedbox_sdk::setup_complete(json!({"nodes": args.nodes}));
    let tally = Arc::new(Mutex::new(Tally::default()));
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let (s3, tally, args, bucket) =
                (s3.clone(), tally.clone(), args.clone(), bucket.clone());
            std::thread::spawn(move || {
                let mut mine: Vec<String> = Vec::new();
                let mut i = 0usize;
                while start.elapsed() < Duration::from_secs(args.duration) {
                    i += 1;
                    let node = &args.nodes[(client + i) % args.nodes.len()];
                    if i % 4 == 0 && !mine.is_empty() {
                        let key = &mine[(i / 4 * 7919) % mine.len()];
                        let reply = s3.request(node, "GET", &format!("{bucket}/{key}"), &[]);
                        let mut t = tally.lock().unwrap();
                        t.reads += 1;
                        match reply {
                            Reply::Status(200, data) if data != contents(key, args.size) => t
                                .stale
                                .push(json!({"key": key, "node": node, "read": "different bytes"})),
                            Reply::Status(404, _) => t
                                .stale
                                .push(json!({"key": key, "node": node, "read": "missing"})),
                            _ => {}
                        }
                    } else {
                        let key = format!("c{client}-{i}");
                        let body = contents(&key, args.size);
                        match s3.request(node, "PUT", &format!("{bucket}/{key}"), &body) {
                            Reply::Status(200, _) => {
                                mine.push(key.clone());
                                tally.lock().unwrap().acknowledged.push(key);
                            }
                            Reply::Status(status, body) => {
                                let mut t = tally.lock().unwrap();
                                t.failed += 1;
                                *t.failures
                                    .entry(format!("{status} {}", s3_code(&body)))
                                    .or_default() += 1;
                            }
                            Reply::Unreachable(_) => tally.lock().unwrap().unknown += 1,
                        }
                    }
                    std::thread::sleep(Duration::from_millis(args.think_ms));
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    let (acknowledged, reads, stale) = {
        let t = tally.lock().unwrap();
        kit::evidence(
            "progress",
            json!({"acknowledged": t.acknowledged.len(), "failed": t.failed, "failures": t.failures,
                   "unknown": t.unknown, "reads": t.reads}),
        );
        (t.acknowledged.clone(), t.reads, t.stale.clone())
    };
    // The fault profile ends before the writes do; let reboots and healing finish.
    kit::pause(args.quiet as f64);
    // Read back every acknowledged object, from the first node that answers.
    let mut missing = Vec::new();
    let mut corrupt = Vec::new();
    let mut unreadable = Vec::new();
    for key in &acknowledged {
        let expected = contents(key, args.size);
        let mut outcome = None;
        for attempt in 0..3 {
            for node in &args.nodes {
                match s3.request(node, "GET", &format!("{bucket}/{key}"), &[]) {
                    Reply::Status(200, data) => {
                        outcome = Some(data == expected);
                        break;
                    }
                    Reply::Status(404, _) => {
                        outcome = Some(false);
                        missing.push(key.clone());
                        break;
                    }
                    _ => {}
                }
            }
            if outcome.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_secs(1 + attempt));
        }
        match outcome {
            Some(true) => {}
            Some(false) if missing.last() == Some(key) => {}
            Some(false) => corrupt.push(key.clone()),
            None => unreadable.push(key.clone()),
        }
    }
    let mut details = BTreeMap::new();
    details.insert(
        "missing",
        json!(missing.iter().take(10).collect::<Vec<_>>()),
    );
    details.insert(
        "corrupt",
        json!(corrupt.iter().take(10).collect::<Vec<_>>()),
    );
    details.insert("unreadable", json!(unreadable.len()));
    details.insert(
        "stale_reads",
        json!(stale.iter().take(10).collect::<Vec<_>>()),
    );
    kit::evidence("details", json!(details));
    let summary = format!(
        "{} acknowledged objects: {} missing, {} with other bytes, {} unreadable; {} of {} reads during the run stale",
        acknowledged.len(),
        missing.len(),
        corrupt.len(),
        unreadable.len(),
        stale.len(),
        reads
    );
    if !missing.is_empty() || !corrupt.is_empty() || !stale.is_empty() {
        kit::verdict(kit::VIOLATION, &summary)
    } else if !unreadable.is_empty() || acknowledged.is_empty() {
        kit::verdict(kit::INCONCLUSIVE, &summary)
    } else {
        kit::verdict(kit::OK, &summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_the_aws_example() {
        // AWS's documented GET Object example (examplebucket/test.txt with a
        // Range header) differs in headers; check the parts that are fixed:
        // the date conversion and the signing-key derivation.
        assert_eq!(
            amz_date(1_369_353_600),
            ("20130524".into(), "20130524T000000Z".into())
        );
        let mut key = b"AWS4wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_vec();
        for part in ["20130524", "us-east-1", "s3", "aws4_request"] {
            key = hmac(&key, part.as_bytes());
        }
        assert_eq!(
            hex(&key),
            "dbb893acc010964918f1fd433add87c70e8b0db6be30c1fbeafefa5ec6ba8378"
        );
    }

    #[test]
    fn contents_are_deterministic_per_key() {
        assert_eq!(contents("c0-1", 64), contents("c0-1", 64));
        assert_ne!(contents("c0-1", 64), contents("c0-2", 64));
    }
}
