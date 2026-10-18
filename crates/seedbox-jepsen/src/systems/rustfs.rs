//! The RustFS object-store oracle: S3 PUTs of uniquely named objects from
//! several clients, each to a rotating node, with reads of acknowledged
//! objects during the run and a read-back of every acknowledged object once
//! faults stop. An acknowledged object that reads back missing, or with bytes
//! other than those written, violates the check (S3 promises strong
//! read-after-write); a read-back that cannot reach the cluster is
//! inconclusive. Contents derive from the key, so no copy is kept. Every
//! server's final listing must then hold every acknowledged key and only keys
//! some client tried to write (S3 listings are strongly consistent too).
//!
//! `--workload register` instead treats a few keys as registers: PUT writes
//! a unique value, DELETE writes "absent", GET reads, from clients spread over
//! the nodes, and every key ends with a read once faults stop. Each key's
//! history must be linearizable ([`RegisterOf`]); a PUT or DELETE without a
//! 2xx reply may or may not have taken effect, unless it was never sent or
//! was refused as malformed or unauthenticated. `--workload cas-register`
//! adds compare-and-set: a GET, then a PUT conditional on the ETag it
//! returned (`If-Match`) or, from absent, `If-None-Match: *`; 412 (or 404 for
//! `If-Match`) is a definite failure.
//!
//! The same oracle runs against keel-s3dd (the `keel-s3` subcommand), one
//! server or several.
//!
//! Requests are signed with AWS Signature Version 4 (path-style, region
//! `us-east-1`, payload hashed) over the kit's HTTP/1.1 client.
use crate::checker::models::{verdict_word, RegisterOp, RegisterRet, Timed};
use crate::kit::{self, http, Result, Rng};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufReader, Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// Unique objects, each checked for survival and contents.
    Objects,
    /// A few overwritten keys, each checked for linearizability.
    Register,
    /// The register workload plus compare-and-set by conditional PUT
    /// (If-Match on a read's ETag, If-None-Match for a create).
    CasRegister,
}

fn workload_name(workload: Workload) -> &'static str {
    match workload {
        Workload::Objects => "objects",
        Workload::Register => "register",
        Workload::CasRegister => "cas-register",
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, value_enum, default_value_t = Workload::Objects)]
    pub workload: Workload,
    /// Register workload: the number of keys.
    #[arg(long, default_value_t = 6)]
    pub keys: usize,
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
    /// Power-fail every server together about this often during the writes,
    /// in seconds; 0 never does.
    #[arg(long, default_value_t = 0.0)]
    pub crash_all_every: f64,
    /// Seconds the servers stay down after a coordinated power failure.
    #[arg(long, default_value_t = 2.0)]
    pub crash_all_down: f64,
    /// 0.05-0.4 s after each coordinated reboot, cut one random server off
    /// from the others for this many seconds while they start up; 0 never does.
    #[arg(long, default_value_t = 0.0)]
    pub startup_partition: f64,
    /// Seconds to wait after the writes before the read-back.
    #[arg(long, default_value_t = 20)]
    pub quiet: u64,
    /// Seconds after the quiet period that every server has to answer a
    /// listing again; one that never does fails the run.
    #[arg(long, default_value_t = 120)]
    pub recover: u64,
    /// Power-cycle every server after the quiet period, before the checks:
    /// a server that stops serving after an I/O error by design (fail-stop)
    /// must then recover from its disk with every acknowledged write.
    #[arg(long)]
    pub reboot_before_check: bool,
    /// Corruption of a server's only copy may destroy data: report lost,
    /// unreadable and unlisted acknowledged objects, and servers that never
    /// recover, without failing on them; fail only on bytes no client wrote
    /// and listings of keys no client wrote.
    #[arg(long)]
    pub integrity_only: bool,
    /// Register workloads: after a write or compare-and-set whose outcome is
    /// unknown, the client waits this many milliseconds, so a server that
    /// answers every write with an ambiguous 500 (one wedged after an I/O
    /// error) leaves a history the checker can still search.
    #[arg(long, default_value_t = 0)]
    pub unknown_backoff_ms: u64,
    /// Register workloads: once a key has this many writes and
    /// compare-and-sets of unknown outcome since the server last answered an
    /// operation on it definitely, clients only read it until one does. This
    /// bounds a key's distinct unknown writes, which the linearizability
    /// search is exponential in, whatever an outage's length. 0 never caps.
    #[arg(long, default_value_t = 8)]
    pub pending_cap: usize,
}

/// What one S3 request came back with.
enum Reply {
    Status(u16, Vec<u8>),
    Unreachable(String),
}

/// One S3 exchange in full, telling a request that never left (no
/// connection: it cannot have taken effect) from one whose answer was lost.
enum Exchange {
    /// Status, body and `ETag` header.
    Answered(u16, Vec<u8>, Option<String>),
    NotSent(String),
    Lost(String),
}

struct S3 {
    args: Args,
}

impl S3 {
    fn request(&self, node: &str, method: &str, path: &str, body: &[u8]) -> Reply {
        match self.send(node, method, path, body, &[]) {
            Exchange::Answered(status, data, _) => Reply::Status(status, data),
            Exchange::NotSent(error) | Exchange::Lost(error) => Reply::Unreachable(error),
        }
    }

    /// One request with `extra` unsigned headers (conditions such as
    /// `If-Match`; SigV4 lets a request carry headers it does not sign).
    fn send(
        &self,
        node: &str,
        method: &str,
        path: &str,
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> Exchange {
        let timeout = Duration::from_secs(self.args.timeout);
        let stream = match http::connect(node, self.args.port, timeout) {
            Ok(stream) => stream,
            Err(error) => return Exchange::NotSent(error.0),
        };
        match self.exchange(stream, node, method, path, body, extra) {
            Ok((status, data, etag)) => Exchange::Answered(status, data, etag),
            Err(error) => Exchange::Lost(error.0),
        }
    }

    fn exchange(
        &self,
        mut stream: std::net::TcpStream,
        node: &str,
        method: &str,
        path: &str,
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> Result<(u16, Vec<u8>, Option<String>)> {
        let host = format!("{node}:{}", self.args.port);
        let headers = sign(
            (&self.args.access_key, &self.args.secret_key),
            method,
            &host,
            path,
            body,
            SystemTime::now(),
        );
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in &headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        for (name, value) in extra {
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
        Ok((head.status, data, head.header("etag").map(str::to_owned)))
    }
}

/// One SigV4-signed S3 request without a body, such as a CreateBucket, for
/// other oracles: its status, or why it got none.
pub(crate) fn signed_request(
    node: &str,
    port: u16,
    keys: (&str, &str),
    method: &str,
    path: &str,
    timeout: Duration,
) -> std::result::Result<u16, String> {
    let host = format!("{node}:{port}");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: 0\r\n"
    );
    for (name, value) in sign(keys, method, &host, path, &[], SystemTime::now()) {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let exchange = || -> Result<u16> {
        let mut stream = http::connect(node, port, timeout)?;
        stream.write_all(head.as_bytes())?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let head = http::read_head(&mut reader)?;
        let mut data = Vec::new();
        http::Body::new(reader, &head).read_to_end(&mut data)?;
        Ok(head.status)
    };
    exchange().map_err(|error| error.0)
}

/// SigV4 headers for one request: `x-amz-date`, `x-amz-content-sha256` and
/// `Authorization`. `target` is the path and any query, already in canonical
/// form (parameters sorted and URI-encoded).
fn sign(
    (access_key, secret_key): (&str, &str),
    method: &str,
    host: &str,
    target: &str,
    body: &[u8],
    at: SystemTime,
) -> Vec<(String, String)> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (date, stamp) = amz_date(seconds);
    let payload = hex(&Sha256::digest(body));
    let canonical = format!(
        "{method}\n{path}\n{query}\nhost:{host}\nx-amz-content-sha256:{payload}\nx-amz-date:{stamp}\n\nhost;x-amz-content-sha256;x-amz-date\n{payload}"
    );
    let scope = format!("{date}/us-east-1/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut key = format!("AWS4{secret_key}").into_bytes();
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
                access_key
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

/// A register value as stored: `v{value}` and padding to `size` bytes. Zero
/// is "absent" (a deleted or never-written key).
fn register_body(value: u32, size: usize) -> Vec<u8> {
    let mut body = format!("v{value}\n").into_bytes();
    body.resize(size.max(body.len()), b'.');
    body
}

/// The value a register body holds; `None` for bytes no client wrote.
fn register_value(body: &[u8], size: usize) -> Option<u32> {
    let text = std::str::from_utf8(body).ok()?;
    let value: u32 = text.strip_prefix('v')?.split_once('\n')?.0.parse().ok()?;
    (register_body(value, size) == body).then_some(value)
}

type KeyHistory = Timed<RegisterOp<u32>, RegisterRet<u32>>;

/// What one register operation saw.
#[derive(Default)]
struct Observed {
    /// A read's value (0 for absent) and the `ETag` it came with.
    read: Option<(u32, Option<String>)>,
    /// A description of bytes no client wrote, if a read saw some.
    unwritten: Option<String>,
    /// A write or compare-and-set whose outcome is unknown.
    unknown: bool,
    /// ... answered 500 (an error that may follow an effect).
    refused: bool,
    /// The server answered definitely: a read, a write or a refused
    /// compare-and-set completed.
    answered: bool,
}

/// One register operation on `key` through `node`, recorded in `history`.
/// A compare-and-set is a conditional PUT: from absent, `If-None-Match: *`;
/// from a value, `If-Match` with the `etag` a read of that value returned
/// (every write's body is unique, so its ETag names it).
fn register_op(
    s3: &S3,
    args: &Args,
    path: &str,
    node: &str,
    op: RegisterOp<u32>,
    etag: Option<&str>,
    history: &Mutex<KeyHistory>,
) -> Observed {
    let id = history.lock().unwrap().invoke(op.clone());
    let (method, body, condition) = match op {
        RegisterOp::Read => ("GET", Vec::new(), None),
        RegisterOp::Write(0) => ("DELETE", Vec::new(), None),
        RegisterOp::Write(value) => ("PUT", register_body(value, args.size), None),
        RegisterOp::Cas(0, to) => (
            "PUT",
            register_body(to, args.size),
            Some(("If-None-Match", "*")),
        ),
        RegisterOp::Cas(_, to) => (
            "PUT",
            register_body(to, args.size),
            Some(("If-Match", etag.unwrap_or("\"\""))),
        ),
    };
    let extra: Vec<(&str, &str)> = condition.into_iter().collect();
    let reply = s3.send(node, method, path, &body, &extra);
    let mut history = history.lock().unwrap();
    let mut observed = Observed::default();
    match (&op, reply) {
        (RegisterOp::Read, Exchange::Answered(200, data, etag)) => {
            match register_value(&data, args.size) {
                Some(value) => {
                    history.complete(id, RegisterRet::Value(Some(value)));
                    observed.read = Some((value, etag));
                    observed.answered = true;
                }
                None => {
                    // Not linearizable with any write; the checker flags it too.
                    history.complete(id, RegisterRet::Value(Some(u32::MAX)));
                    observed.unwritten = Some(format!(
                        "{node} returned {} bytes no client wrote: {:?}",
                        data.len(),
                        String::from_utf8_lossy(&data[..data.len().min(32)])
                    ));
                }
            }
        }
        (RegisterOp::Read, Exchange::Answered(404, ..)) => {
            history.complete(id, RegisterRet::Value(Some(0)));
            observed.read = Some((0, None));
            observed.answered = true;
        }
        // A failed read has no effect.
        (RegisterOp::Read, _) => history.fail(id),
        (RegisterOp::Write(_), Exchange::Answered(200 | 204, ..)) => {
            history.complete(id, RegisterRet::Wrote);
            observed.answered = true;
        }
        (RegisterOp::Cas(..), Exchange::Answered(200, ..)) => {
            history.complete(id, RegisterRet::Cas(true));
            observed.answered = true;
        }
        // The condition did not hold: the key held another value, or (for
        // If-Match, answered 404) none.
        (RegisterOp::Cas(..), Exchange::Answered(412, ..)) => {
            history.complete(id, RegisterRet::Cas(false));
            observed.answered = true;
        }
        (RegisterOp::Cas(from, _), Exchange::Answered(404, ..)) if *from != 0 => {
            history.complete(id, RegisterRet::Cas(false));
            observed.answered = true;
        }
        // Refused before it could take effect: never sent, or rejected as a
        // malformed or unauthenticated request.
        (_, Exchange::NotSent(_) | Exchange::Answered(400 | 403, ..)) => history.fail(id),
        // A bare 503 with no error document is keel-s3dd refusing a request
        // before running it (its store still recovering, or no task slot);
        // S3 errors that may follow an effect carry a document.
        (_, Exchange::Answered(503, data, _)) if data.is_empty() => history.fail(id),
        // Anything else may or may not have taken effect: leave it pending.
        (_, Exchange::Answered(500, ..)) => {
            observed.unknown = true;
            observed.refused = true;
        }
        _ => observed.unknown = true,
    }
    observed
}

fn register(s3: Arc<S3>, args: &Args, bucket: &str) -> u8 {
    let keys: Vec<String> = (0..args.keys).map(|k| format!("{bucket}/r{k}")).collect();
    let histories: Arc<Vec<Mutex<KeyHistory>>> =
        Arc::new(keys.iter().map(|_| Mutex::new(KeyHistory::new())).collect());
    let unwritten = Arc::new(Mutex::new(Vec::<String>::new()));
    // Reads whose value came without an ETag, so no CAS could follow them.
    let no_etag = Arc::new(Mutex::new(0u64));
    // Per key, unknown writes since the server last answered definitely.
    let unknown: Arc<Vec<std::sync::atomic::AtomicUsize>> =
        Arc::new(keys.iter().map(|_| Default::default()).collect());
    // When writes were answered with an ambiguous 500.
    let refusals = Arc::new(Mutex::new(Vec::<f64>::new()));
    // Operations turned into reads by --pending-cap.
    let capped = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Every register starts absent: write that before the clients start, so
    // the model's initial state (no value) is never observed.
    for history in histories.iter() {
        let mut history = history.lock().unwrap();
        let id = history.invoke(RegisterOp::Write(0));
        history.complete(id, RegisterRet::Wrote);
    }
    let seed = kit::rng::entropy_seed();
    kit::evidence(
        "start",
        json!({"workload": workload_name(args.workload), "keys": args.keys, "rng_seed": seed}),
    );
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let (s3, args, keys, histories, unwritten, no_etag) = (
                s3.clone(),
                args.clone(),
                keys.clone(),
                histories.clone(),
                unwritten.clone(),
                no_etag.clone(),
            );
            let (unknown, refusals, capped) = (unknown.clone(), refusals.clone(), capped.clone());
            std::thread::spawn(move || {
                let mut rng = Rng::stream(seed, client as u64);
                let mut next = 1u32;
                while start.elapsed() < Duration::from_secs(args.duration) {
                    let k = rng.below(keys.len() as u64) as usize;
                    let node = rng.choice(&args.nodes).clone();
                    // Read, write, delete and (conditional writes only)
                    // compare-and-set.
                    let weights: &[f64] = match args.workload {
                        Workload::CasRegister => &[3.0, 2.0, 1.0, 4.0],
                        _ => &[5.0, 4.0, 1.0],
                    };
                    // Unique per client: client in the low bits.
                    let value = next * args.clients as u32 + client as u32 + 1;
                    let record = |observed: Observed| {
                        if let Some(bad) = &observed.unwritten {
                            unwritten
                                .lock()
                                .unwrap()
                                .push(format!("{}: {bad}", keys[k]));
                        }
                        use std::sync::atomic::Ordering::Relaxed;
                        if observed.refused {
                            refusals.lock().unwrap().push(kit::now());
                        }
                        if observed.answered {
                            unknown[k].store(0, Relaxed);
                        } else if observed.unknown {
                            unknown[k].fetch_add(1, Relaxed);
                        }
                        if observed.unknown && args.unknown_backoff_ms > 0 {
                            std::thread::sleep(Duration::from_millis(args.unknown_backoff_ms));
                        }
                        observed.read
                    };
                    let at_cap = args.pending_cap > 0
                        && unknown[k].load(std::sync::atomic::Ordering::Relaxed)
                            >= args.pending_cap;
                    let choice = rng.weighted(weights);
                    if at_cap && choice != 0 {
                        capped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let op = match if at_cap { 0 } else { choice } {
                        0 => RegisterOp::Read,
                        1 => RegisterOp::Write(value),
                        2 => RegisterOp::Write(0),
                        _ => {
                            // Read the current value and its ETag first.
                            let read = record(register_op(
                                &s3,
                                &args,
                                &keys[k],
                                &node,
                                RegisterOp::Read,
                                None,
                                &histories[k],
                            ));
                            match read {
                                Some((from, etag)) if from == 0 || etag.is_some() => {
                                    record(register_op(
                                        &s3,
                                        &args,
                                        &keys[k],
                                        &node,
                                        RegisterOp::Cas(from, value),
                                        etag.as_deref(),
                                        &histories[k],
                                    ));
                                    next += 1;
                                }
                                Some(_) => *no_etag.lock().unwrap() += 1,
                                None => {}
                            }
                            std::thread::sleep(Duration::from_millis(args.think_ms));
                            continue;
                        }
                    };
                    if matches!(op, RegisterOp::Write(v) if v != 0) {
                        next += 1;
                    }
                    record(register_op(
                        &s3,
                        &args,
                        &keys[k],
                        &node,
                        op,
                        None,
                        &histories[k],
                    ));
                    std::thread::sleep(Duration::from_millis(args.think_ms));
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    kit::pause(args.quiet as f64);
    reboot_before_check(args);
    let (live, down) = recover(&s3, args, bucket);
    // A final read of every key, retried on other nodes until one answers.
    for (k, key) in keys.iter().enumerate().filter(|_| !live.is_empty()) {
        'key: for attempt in 0..3u64 {
            for node in &live {
                let observed =
                    register_op(&s3, args, key, node, RegisterOp::Read, None, &histories[k]);
                if let Some(bad) = observed.unwritten {
                    unwritten
                        .lock()
                        .unwrap()
                        .push(format!("{key} (final): {bad}"));
                }
                let answered = matches!(
                    histories[k]
                        .lock()
                        .unwrap()
                        .operations()
                        .last()
                        .map(|o| &o.outcome),
                    Some(wgl::OpOutcome::Completed { .. })
                );
                if answered {
                    break 'key;
                }
            }
            std::thread::sleep(Duration::from_secs(1 + attempt));
        }
    }
    // Record each key's size first: the check below is the expensive part.
    let sizes: Vec<_> = histories
        .iter()
        .map(|h| {
            let h = h.lock().unwrap();
            let ops = h.operations();
            let pending = ops
                .iter()
                .filter(|o| matches!(o.outcome, wgl::OpOutcome::Pending))
                .count();
            // Compare-and-sets by outcome: applied, refused (the condition
            // did not hold), other.
            let cas = |want: Option<bool>| {
                ops.iter()
                    .filter(|o| matches!(o.op, RegisterOp::Cas(..)))
                    .filter(|o| match (&o.outcome, want) {
                        (wgl::OpOutcome::Completed { ret, .. }, Some(want)) => {
                            *ret == RegisterRet::Cas(want)
                        }
                        (wgl::OpOutcome::Completed { .. }, None) => false,
                        (_, want) => want.is_none(),
                    })
                    .count()
            };
            json!({"ops": ops.len(), "pending": pending,
                   "cas": [cas(Some(true)), cas(Some(false)), cas(None)]})
        })
        .collect();
    kit::evidence(
        "histories",
        json!({"keys": sizes, "reads_without_etag": *no_etag.lock().unwrap(),
               "capped_to_reads": capped.load(std::sync::atomic::Ordering::Relaxed)}),
    );
    let mut violations = Vec::new();
    let mut exhausted = Vec::new();
    let mut counts = json!({});
    for (key, history) in keys.iter().zip(histories.iter()) {
        let history = history.lock().unwrap();
        let operations = history.operations();
        let pending = operations
            .iter()
            .filter(|o| matches!(o.outcome, wgl::OpOutcome::Pending))
            .count();
        let verdict = crate::checker::models::check_register(key, operations);
        counts[key] =
            json!({"ops": operations.len(), "pending": pending, "verdict": verdict_word(&verdict)});
        match verdict {
            wgl::Verdict::Linearizable => {}
            wgl::Verdict::NotLinearizable(e) => {
                let blocked = e.blocked_on.map(|i| {
                    let o = &operations[i];
                    format!("{:?} -> {:?}", o.op, o.outcome)
                });
                kit::evidence(
                    "nonlinearizable",
                    json!({"key": key, "blocked_on": blocked, "linearized": e.longest_prefix.len()}),
                );
                history.print(key);
                violations.push(key.clone());
            }
            wgl::Verdict::BudgetExhausted(_) => exhausted.push(key.clone()),
        }
    }
    let unwritten = unwritten.lock().unwrap().clone();
    kit::evidence(
        "details",
        json!({"keys": counts, "unwritten": unwritten.iter().take(10).collect::<Vec<_>>()}),
    );
    let summary = format!(
        "{} of {} keys not linearizable, {} undecided; {} reads of bytes no client wrote{}{}",
        violations.len(),
        keys.len(),
        exhausted.len(),
        unwritten.len(),
        refusal_windows(&refusals.lock().unwrap()),
        unrecovered(&down)
    );
    let lost = !violations.is_empty() || !down.is_empty();
    // An undecided key is a failed test (wgl treats budget exhaustion so),
    // not an unevaluated property.
    if !unwritten.is_empty() || !exhausted.is_empty() || (lost && !args.integrity_only) {
        kit::verdict(kit::VIOLATION, &summary)
    } else if lost {
        kit::verdict(kit::INCONCLUSIVE, &summary)
    } else {
        kit::verdict(kit::OK, &summary)
    }
}

/// The spans in which writes were answered with an ambiguous 500 (times in
/// seconds; a gap of more than 5 s starts a new span), recorded as
/// `fail-stop` evidence and described for the verdict: a server that stops
/// accepting writes after an I/O error is failing stop, which is allowed as
/// long as nothing acknowledged is lost.
fn refusal_windows(times: &[f64]) -> String {
    let mut times = times.to_vec();
    times.sort_by(f64::total_cmp);
    let mut spans: Vec<(f64, f64, usize)> = Vec::new();
    for t in times {
        match spans.last_mut() {
            Some((_, end, count)) if t - *end <= 5.0 => {
                *end = t;
                *count += 1;
            }
            _ => spans.push((t, t, 1)),
        }
    }
    let long: Vec<_> = spans.iter().filter(|(_, _, n)| *n >= 3).collect();
    if long.is_empty() {
        return String::new();
    }
    kit::evidence(
        "fail-stop",
        json!({"spans": long.iter().map(|(a, b, n)| json!({"from": a, "to": b, "refusals": n})).collect::<Vec<_>>()}),
    );
    let text: Vec<String> = long
        .iter()
        .map(|(a, b, n)| format!("{a:.1}-{b:.1} s ({n} refusals)"))
        .collect();
    format!(
        "; writes answered with ambiguous 500s (fail-stop) at {}",
        text.join(", ")
    )
}

/// With `--reboot-before-check`, power-fail every server together and boot
/// them a second later.
fn reboot_before_check(args: &Args) {
    if !args.reboot_before_check {
        return;
    }
    let nodes: Vec<&str> = args.nodes.iter().map(String::as_str).collect();
    match kit::connect_world(None) {
        Ok(mut ctl) => {
            if let Err(error) = crate::nemesis::crash_all(
                &mut ctl,
                &mut crate::nemesis::Sleep,
                &nodes,
                1.0,
                json!({"before_check": true}),
            ) {
                kit::evidence("nemesis-failed", json!({"error": error.0}));
            }
        }
        Err(error) => kit::evidence("nemesis-unavailable", json!({"error": error.0})),
    }
}

/// Wait up to `--recover` seconds for every server to answer a one-key
/// listing, and split them into those that did and those that never did.
fn recover(s3: &S3, args: &Args, bucket: &str) -> (Vec<String>, Vec<String>) {
    let start = Instant::now();
    let mut waiting = args.nodes.clone();
    let mut last = BTreeMap::new();
    while !waiting.is_empty() {
        waiting.retain(|node| {
            match s3.request(
                node,
                "GET",
                &format!("{bucket}?list-type=2&max-keys=1"),
                &[],
            ) {
                Reply::Status(200, _) => false,
                Reply::Status(status, body) => {
                    last.insert(node.clone(), format!("HTTP {status} {}", s3_code(&body)));
                    true
                }
                Reply::Unreachable(error) => {
                    last.insert(node.clone(), error);
                    true
                }
            }
        });
        if waiting.is_empty() || start.elapsed() >= Duration::from_secs(args.recover) {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    for node in &waiting {
        kit::evidence(
            "unrecovered",
            json!({"node": node, "waited": start.elapsed().as_secs_f64(), "last": last.get(node)}),
        );
    }
    let live = args
        .nodes
        .iter()
        .filter(|n| !waiting.contains(n))
        .cloned()
        .collect();
    (live, waiting)
}

/// The verdict suffix naming servers that never recovered.
fn unrecovered(down: &[String]) -> String {
    if down.is_empty() {
        String::new()
    } else {
        format!(
            "; {} never answered again after the faults ended",
            down.join(", ")
        )
    }
}

/// Every key in `bucket` as `node` lists it (ListObjectsV2, a page at a time).
fn list(s3: &S3, node: &str, bucket: &str) -> std::result::Result<Vec<String>, String> {
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    loop {
        // Parameters in canonical (sorted) order.
        let mut query = String::new();
        if let Some(token) = &token {
            query.push_str(&format!("continuation-token={}&", uri_encode(token)));
        }
        query.push_str("list-type=2&max-keys=1000");
        match s3.request(node, "GET", &format!("{bucket}?{query}"), &[]) {
            Reply::Status(200, body) => {
                let text = String::from_utf8_lossy(&body);
                keys.extend(xml_texts(&text, "Key"));
                let truncated =
                    xml_texts(&text, "IsTruncated").first().map(String::as_str) == Some("true");
                match xml_texts(&text, "NextContinuationToken").pop() {
                    Some(next) if truncated => token = Some(next),
                    _ if truncated => return Err("truncated listing without a token".into()),
                    _ => return Ok(keys),
                }
            }
            Reply::Status(status, body) => return Err(format!("HTTP {status} {}", s3_code(&body))),
            Reply::Unreachable(error) => return Err(error),
        }
    }
}

/// The unescaped text of every `<tag>` element.
fn xml_texts(text: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once(open.as_str()) {
        let Some((inner, tail)) = after.split_once(close.as_str()) else {
            break;
        };
        out.push(
            inner
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&amp;", "&"),
        );
        rest = tail;
    }
    out
}

/// URI encoding as SigV4 specifies it: RFC 3986 unreserved characters stay.
fn uri_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
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
    /// Every key a PUT was sent for, acknowledged or not.
    attempted: Vec<String>,
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
    seedbox_sdk::setup_complete(
        json!({"nodes": args.nodes, "workload": format!("{:?}", args.workload)}),
    );
    if args.crash_all_every > 0.0 {
        // A healthy restart finishes its storage setup within about 0.05 s
        // and its bucket metadata migration about 0.35 s in, which the
        // startup partition's 0.05-0.4 s delay straddles.
        crate::nemesis::crash_all_repeatedly(
            args.nodes.clone(),
            args.crash_all_every,
            args.crash_all_down,
            args.startup_partition,
            args.duration as f64,
        );
    }
    match args.workload {
        Workload::Objects => objects(s3, &args, &bucket),
        Workload::Register | Workload::CasRegister => register(s3, &args, &bucket),
    }
}

fn objects(s3: Arc<S3>, args: &Args, bucket: &str) -> u8 {
    let tally = Arc::new(Mutex::new(Tally::default()));
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let (s3, tally, args, bucket) =
                (s3.clone(), tally.clone(), args.clone(), bucket.to_owned());
            std::thread::spawn(move || {
                let mut mine: Vec<String> = Vec::new();
                let mut i = 0usize;
                while start.elapsed() < Duration::from_secs(args.duration) {
                    i += 1;
                    let node = &args.nodes[(client + i) % args.nodes.len()];
                    if i.is_multiple_of(4) && !mine.is_empty() {
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
                        tally.lock().unwrap().attempted.push(key.clone());
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
    let (attempted, acknowledged, reads, stale) = {
        let t = tally.lock().unwrap();
        kit::evidence(
            "progress",
            json!({"acknowledged": t.acknowledged.len(), "failed": t.failed, "failures": t.failures,
                   "unknown": t.unknown, "reads": t.reads}),
        );
        (
            t.attempted.clone(),
            t.acknowledged.clone(),
            t.reads,
            t.stale.clone(),
        )
    };
    // The fault profile ends before the writes do; let reboots and healing finish.
    kit::pause(args.quiet as f64);
    reboot_before_check(args);
    let (live, down) = recover(&s3, args, bucket);
    // Read back every acknowledged object, from the first node that answers.
    let mut missing = Vec::new();
    let mut corrupt = Vec::new();
    let mut unreadable = Vec::new();
    // With no server answering, every object is unreadable: skip the retries.
    if live.is_empty() {
        unreadable.clone_from(&acknowledged);
    }
    // Once many keys in a row are unreadable, try each remaining key once:
    // a server that answers every read with an error would otherwise cost
    // seconds of retries per object.
    let mut unreadable_run = 0;
    for key in acknowledged.iter().filter(|_| !live.is_empty()) {
        let expected = contents(key, args.size);
        let mut outcome = None;
        let attempts = if unreadable_run >= 10 { 1 } else { 3 };
        for attempt in 0..attempts {
            for node in &live {
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
            if outcome.is_some() || attempt + 1 == attempts {
                break;
            }
            std::thread::sleep(Duration::from_secs(1 + attempt));
        }
        unreadable_run = if outcome.is_none() {
            unreadable_run + 1
        } else {
            0
        };
        match outcome {
            Some(true) => {}
            Some(false) if missing.last() == Some(key) => {}
            Some(false) => corrupt.push(key.clone()),
            None => unreadable.push(key.clone()),
        }
    }
    // Every server's listing must hold every acknowledged key, and only keys
    // some client tried to write.
    let attempted: BTreeSet<&str> = attempted.iter().map(String::as_str).collect();
    let mut unlisted = Vec::new();
    let mut phantom = Vec::new();
    let mut unlistable = 0;
    for node in &live {
        let mut listed = Err(String::new());
        for attempt in 0..3 {
            listed = list(&s3, node, bucket);
            if listed.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_secs(1 + attempt));
        }
        let listed = match listed {
            Ok(keys) => keys,
            Err(error) => {
                kit::evidence("unlistable", json!({"node": node, "error": error}));
                unlistable += 1;
                continue;
            }
        };
        let set: BTreeSet<&str> = listed.iter().map(String::as_str).collect();
        for key in &acknowledged {
            if !set.contains(key.as_str()) {
                unlisted.push(json!({"node": node, "key": key}));
            }
        }
        for key in set.difference(&attempted) {
            phantom.push(json!({"node": node, "key": key}));
        }
    }
    let mut details = BTreeMap::new();
    details.insert("unlisted", json!(unlisted.len()));
    details.insert(
        "unlisted_sample",
        json!(unlisted.iter().take(10).collect::<Vec<_>>()),
    );
    details.insert(
        "phantom",
        json!(phantom.iter().take(10).collect::<Vec<_>>()),
    );
    details.insert("unlistable_nodes", json!(unlistable));
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
        "{} acknowledged objects: {} missing, {} with other bytes, {} unreadable; {} of {} reads during the run stale; {} unlisted and {} phantom listings{}",
        acknowledged.len(),
        missing.len(),
        corrupt.len(),
        unreadable.len(),
        stale.len(),
        reads,
        unlisted.len(),
        phantom.len(),
        unrecovered(&down)
    );
    // Reads during the run that returned bytes no client wrote.
    let wrong = stale.iter().any(|s| s["read"] == "different bytes");
    let lost = !missing.is_empty() || !down.is_empty() || !stale.is_empty() || !unlisted.is_empty();
    if !corrupt.is_empty() || !phantom.is_empty() || wrong || (lost && !args.integrity_only) {
        kit::verdict(kit::VIOLATION, &summary)
    } else if lost || !unreadable.is_empty() || unlistable > 0 || acknowledged.is_empty() {
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
    fn register_bodies_round_trip_and_reject_other_bytes() {
        assert_eq!(register_value(&register_body(42, 64), 64), Some(42));
        assert_eq!(register_value(&register_body(42, 64), 32), None);
        let mut body = register_body(42, 64);
        body[40] = b'x';
        assert_eq!(register_value(&body, 64), None);
    }

    #[test]
    fn signature_matches_the_aws_list_objects_example() {
        // AWS's documented SigV4 example: GET Bucket (List Objects) with
        // max-keys=2 and prefix=J, at 2013-05-24T00:00:00Z.
        let headers = sign(
            (
                "AKIAIOSFODNN7EXAMPLE",
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            ),
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/?max-keys=2&prefix=J",
            &[],
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_369_353_600),
        );
        assert!(headers[2].1.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn listings_parse_keys_and_tokens() {
        let page = "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>c0-1</Key></Contents>\
                    <Contents><Key>a&amp;b</Key></Contents><NextContinuationToken>x/y=</NextContinuationToken></ListBucketResult>";
        assert_eq!(xml_texts(page, "Key"), ["c0-1", "a&b"]);
        assert_eq!(uri_encode("x/y="), "x%2Fy%3D");
    }

    #[test]
    fn contents_are_deterministic_per_key() {
        assert_eq!(contents("c0-1", 64), contents("c0-1", 64));
        assert_ne!(contents("c0-1", 64), contents("c0-2", 64));
    }
}
