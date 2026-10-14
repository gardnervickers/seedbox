//! Jepsen's TigerBeetle 0.16.11 findings as a Seedbox oracle.
//!
//! Every client session is a `tb-worker` child process with its own
//! TigerBeetle C client ([`super::tb_client`]): a 0.16.11 client panics, and
//! takes its process with it, when the cluster evicts its session
//! (tigerbeetle#2484), so a session must not share the oracle's process. The
//! oracle sends a worker one JSON request per line and reads one reply per
//! line. A request that times out, or whose worker dies, is indeterminate
//! (`info`); the oracle then kills that worker and continues with a new one,
//! which is a new session. Nothing is retried.
//!
//! Workloads (`--workload`):
//!
//! * `query` (tigerbeetle#2544): accounts over a few ledgers, codes and user
//!   data values; transfers between Zipf-chosen accounts of one ledger; and
//!   `query_transfers`, `query_accounts` and `get_account_transfers` with one
//!   to three filter fields, random limits and directions. After the run the
//!   oracle reads every object's timestamp and checks each result against the
//!   model ([`crate::checker::ledger`]), then pages through every filter
//!   combination and checks those results exactly.
//! * `reformat` (tigerbeetle#2767): transfers while one replica is cut off; the
//!   two others hold every acknowledged transfer. Both are killed, one is
//!   reformatted (`--replace format`, the undocumented replacement procedure)
//!   or recovered (`--replace recover`, 0.16.43's `tigerbeetle recover`), and
//!   it rejoins with the replica that was cut off before the other restarts.
//!   Every acknowledged transfer must survive.
//! * `timeout` (tigerbeetle#206): kill every replica and submit a transfer.
//!   The request never fails and never times out; after the replicas return
//!   it commits.
//! * `sessions` (tigerbeetle#2484): open more sessions than the cluster's 64;
//!   the evicted clients panic.
use super::tb_client::{
    self, bytes_of, values_of, Account, AccountFilter, Client, CreateResult, Operation,
    QueryFilter, Transfer,
};
use super::tigerbeetle_node;
use crate::checker::ledger::{self, Fields, Filter, Model, Object, Query, Returned};
use crate::kit::{self, evidence, verdict, Rng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, value_enum, default_value = "query")]
    pub workload: Workload,
    /// Replica machines, in replica order.
    #[arg(long, value_delimiter = ',', default_value = "n1,n2,n3")]
    pub nodes: Vec<String>,
    #[arg(long, default_value_t = 3000)]
    pub port: u16,
    /// Replica addresses as `ip:port,...` instead of resolving --nodes (native runs).
    #[arg(long)]
    pub addresses: Option<String>,
    /// The node agents' command port; 0 when no agents run (native runs).
    #[arg(long, default_value_t = 4599)]
    pub agent_port: u16,
    #[arg(long, default_value_t = 0)]
    pub cluster: u128,
    /// The client library release every session uses.
    #[arg(long, value_enum, default_value = "0.16.11")]
    pub client: tb_client::Release,
    /// Concurrent client sessions.
    #[arg(long, default_value_t = 8)]
    pub clients: usize,
    /// Seconds of workload.
    #[arg(long, default_value_t = 60.0)]
    pub duration: f64,
    #[arg(long, default_value_t = 200)]
    pub accounts: usize,
    #[arg(long, default_value_t = 2)]
    pub ledgers: u32,
    #[arg(long, default_value_t = 4)]
    pub codes: u16,
    /// Seconds per transfer user_data_64 value: it counts epochs of the run,
    /// so its index is clustered in time; 0 draws it at random.
    #[arg(long, default_value_t = 0.0)]
    pub epoch: f64,
    /// Distinct transfer user_data_32 values: many make each value sparse, so
    /// a two-field intersection skips far ahead in the other index.
    #[arg(long, default_value_t = 4)]
    pub sparse: u32,
    /// Query filters of one and two fields sampled for the final queries.
    #[arg(long, default_value_t = 1500)]
    pub final_filters: usize,
    /// Transfers carry no user data and amount 1, so only the account, ledger
    /// and code indexes change much: the data file grows far less.
    #[arg(long)]
    pub lean: bool,
    /// Transfers to create first, in full batches and without queries; the
    /// workload's --duration starts after them.
    #[arg(long, default_value_t = 0)]
    pub preload: usize,
    /// Transfers per preload request (at most the server's request limit / 128).
    #[arg(long, default_value_t = 250)]
    pub preload_batch: usize,
    /// Pages per filter and direction in the final queries.
    #[arg(long, default_value_t = 40)]
    pub max_pages: usize,
    /// Distinct user_data_32 and user_data_64 values.
    #[arg(long, default_value_t = 4)]
    pub user_data: u32,
    /// Zipf exponent for choosing accounts.
    #[arg(long, default_value_t = 1.0)]
    pub zipf: f64,
    /// Most transfers in one create_transfers request.
    #[arg(long, default_value_t = 8)]
    pub batch: usize,
    /// Fraction of operations that are queries.
    #[arg(long, default_value_t = 0.25)]
    pub query_fraction: f64,
    /// Each client waits a random 0 to this many milliseconds between requests.
    #[arg(long, default_value_t = 0)]
    pub think_ms: u64,
    /// Seconds before a request counts as indeterminate.
    #[arg(long, default_value_t = 30.0)]
    pub timeout: f64,
    /// Seconds the oracle waits for the cluster to serve.
    #[arg(long, default_value_t = 300.0)]
    pub ready_timeout: f64,
    /// Ids per lookup in the final read.
    /// (0.16.11's release build panicked on backups with lookups of 1000 ids
    /// under --development, natively on macOS.)
    #[arg(long, default_value_t = 100)]
    pub lookup_batch: usize,
    /// Page size of the final queries [default: random per filter].
    #[arg(long)]
    pub page: Option<u32>,
    /// reformat: how the killed replica is replaced.
    #[arg(long, value_enum, default_value = "format")]
    pub replace: Replace,
    /// reformat: the cut, the kill and the rejoin, in seconds from the workload start.
    #[arg(long, default_value_t = 10.0)]
    pub cut_at: f64,
    #[arg(long, default_value_t = 5.0)]
    pub cut_for: f64,
    /// reformat: the replica (index into --nodes) to cut off [default: random].
    #[arg(long)]
    pub cut_node: Option<usize>,
    /// reformat: the replica to replace, one of the other two [default: random].
    #[arg(long)]
    pub replace_node: Option<usize>,
    /// reformat: seconds the replaced replica and the cut-off one run alone.
    #[arg(long, default_value_t = 15.0)]
    pub alone_for: f64,
    /// timeout: seconds every replica stays down.
    #[arg(long, default_value_t = 60.0)]
    pub hold: f64,
    /// sessions: client sessions to open first (a replica keeps 64).
    #[arg(long, default_value_t = 64)]
    pub sessions: usize,
    /// sessions: sessions to replace afterwards, each evicting the oldest.
    #[arg(long, default_value_t = 5)]
    pub evictions: usize,
    /// Workload seed [default: the guest's entropy, which a Seedbox seed fixes].
    #[arg(long)]
    pub seed: Option<u64>,
    #[arg(long, default_value_t = 5)]
    pub examples: usize,
    #[arg(long, default_value = "")]
    pub label: String,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// Multi-field queries checked against the model (tigerbeetle#2544).
    Query,
    /// Replace a replica's data file under faults (tigerbeetle#2767).
    Reformat,
    /// A request while every replica is down (tigerbeetle#206).
    Timeout,
    /// More client sessions than the cluster keeps (tigerbeetle#2484).
    Sessions,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// `tigerbeetle format` a new data file.
    Format,
    /// `tigerbeetle recover` a new data file (0.16.43 and later).
    Recover,
}

// ------------------------------------------------------------ the worker

/// A query's filter on the wire.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Selection {
    pub filter: Filter,
    pub limit: u32,
    pub reversed: bool,
}

// Externally tagged: serde's internally tagged enums cannot carry u128.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    CreateAccounts { accounts: Vec<Account> },
    CreateTransfers { transfers: Vec<Transfer> },
    LookupAccounts { ids: Vec<u128> },
    LookupTransfers { ids: Vec<u128> },
    QueryTransfers { selection: Selection },
    QueryAccounts { selection: Selection },
    GetAccountTransfers { selection: Selection },
}

impl Request {
    fn name(&self) -> &'static str {
        match self {
            Self::CreateAccounts { .. } => "create_accounts",
            Self::CreateTransfers { .. } => "create_transfers",
            Self::LookupAccounts { .. } => "lookup_accounts",
            Self::LookupTransfers { .. } => "lookup_transfers",
            Self::QueryTransfers { .. } => "query_transfers",
            Self::QueryAccounts { .. } => "query_accounts",
            Self::GetAccountTransfers { .. } => "get_account_transfers",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Created {
        results: Vec<CreateResult>,
    },
    Accounts {
        accounts: Vec<Account>,
    },
    Transfers {
        transfers: Vec<Transfer>,
    },
    /// The packet completed with a status other than ok.
    Status {
        status: String,
    },
}

#[derive(clap::Args, Debug, Clone)]
pub struct WorkerArgs {
    #[arg(long)]
    pub addresses: String,
    /// The client library release.
    #[arg(long, value_enum, default_value = "0.16.11")]
    pub client: tb_client::Release,
    #[arg(long, default_value_t = 0)]
    pub cluster: u128,
}

fn query_filter(s: &Selection) -> QueryFilter {
    QueryFilter {
        user_data_128: s.filter.user_data_128,
        user_data_64: s.filter.user_data_64,
        user_data_32: s.filter.user_data_32,
        ledger: s.filter.ledger,
        code: s.filter.code,
        timestamp_min: s.filter.timestamp_min,
        timestamp_max: s.filter.timestamp_max,
        limit: s.limit,
        flags: if s.reversed {
            tb_client::QUERY_FILTER_REVERSED
        } else {
            0
        },
        ..QueryFilter::default()
    }
}

fn account_filter(s: &Selection) -> AccountFilter {
    let mut flags = 0;
    if s.filter.debits {
        flags |= tb_client::ACCOUNT_FILTER_DEBITS;
    }
    if s.filter.credits {
        flags |= tb_client::ACCOUNT_FILTER_CREDITS;
    }
    if s.reversed {
        flags |= tb_client::FILTER_REVERSED_ACCOUNT;
    }
    AccountFilter {
        account_id: s.filter.account,
        user_data_128: s.filter.user_data_128,
        user_data_64: s.filter.user_data_64,
        user_data_32: s.filter.user_data_32,
        code: s.filter.code,
        timestamp_min: s.filter.timestamp_min,
        timestamp_max: s.filter.timestamp_max,
        limit: s.limit,
        flags,
        ..AccountFilter::default()
    }
}

/// Serve requests from stdin on one client session until stdin closes.
pub fn worker(args: WorkerArgs) -> u8 {
    let client = match Client::connect(args.client, args.cluster, &args.addresses) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("tb-worker: {error}");
            return 2;
        }
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("tb-worker: bad request: {error}");
                return 2;
            }
        };
        let (operation, data) = match &request {
            Request::CreateAccounts { accounts } => (Operation::CreateAccounts, bytes_of(accounts)),
            Request::CreateTransfers { transfers } => {
                (Operation::CreateTransfers, bytes_of(transfers))
            }
            Request::LookupAccounts { ids } => (Operation::LookupAccounts, bytes_of(ids)),
            Request::LookupTransfers { ids } => (Operation::LookupTransfers, bytes_of(ids)),
            Request::QueryTransfers { selection } => (
                Operation::QueryTransfers,
                bytes_of(&[query_filter(selection)]),
            ),
            Request::QueryAccounts { selection } => (
                Operation::QueryAccounts,
                bytes_of(&[query_filter(selection)]),
            ),
            Request::GetAccountTransfers { selection } => (
                Operation::GetAccountTransfers,
                bytes_of(&[account_filter(selection)]),
            ),
        };
        let Ok((status, body)) = client.submit(operation, data).recv() else {
            eprintln!("tb-worker: completion channel closed");
            return 2;
        };
        let reply = if status != 0 {
            Reply::Status {
                status: client.release.packet_status(status).into(),
            }
        } else {
            match operation {
                Operation::CreateAccounts | Operation::CreateTransfers => Reply::Created {
                    results: values_of(&body),
                },
                Operation::LookupAccounts | Operation::QueryAccounts => Reply::Accounts {
                    accounts: values_of(&body),
                },
                _ => Reply::Transfers {
                    transfers: values_of(&body),
                },
            }
        };
        let text = serde_json::to_string(&reply).unwrap_or_default();
        if writeln!(stdout, "{text}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
    0
}

// ------------------------------------------------------------ sessions

/// Why a request has no reply.
#[derive(Clone, Debug)]
pub enum Lost {
    /// No reply within the timeout; the request may still commit.
    Timeout,
    /// The worker exited: its exit status and the last lines it wrote to stderr.
    Died(Value),
}

impl Lost {
    fn json(&self) -> Value {
        match self {
            Self::Timeout => json!("timeout"),
            Self::Died(exit) => json!({"died": exit}),
        }
    }
}

/// One client session: a `tb-worker` child.
pub struct Session {
    pub number: usize,
    child: Child,
    stdin: ChildStdin,
    replies: mpsc::Receiver<String>,
    stderr: Arc<Mutex<Vec<String>>>,
    pub requests: u64,
}

impl Session {
    pub fn spawn(
        number: usize,
        addresses: &str,
        cluster: u128,
        client: tb_client::Release,
    ) -> kit::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .args([
                "tb-worker",
                "--addresses",
                addresses,
                "--client",
                client.name(),
                "--cluster",
            ])
            .arg(cluster.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("worker stdin")?;
        let stdout = child.stdout.take().ok_or("worker stdout")?;
        let stderr_pipe = child.stderr.take().ok_or("worker stderr")?;
        let (tx, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let tail = stderr.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                let Ok(line) = line else { break };
                eprintln!("TBWORKER {number} {line}");
                let mut tail = tail.lock().unwrap();
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
        });
        Ok(Self {
            number,
            child,
            stdin,
            replies,
            stderr,
            requests: 0,
        })
    }

    /// Send a request without waiting.
    pub fn send(&mut self, request: &Request) -> Result<(), Lost> {
        self.requests += 1;
        let text = serde_json::to_string(request).unwrap_or_default();
        if writeln!(self.stdin, "{text}")
            .and_then(|()| self.stdin.flush())
            .is_err()
        {
            return Err(self.died());
        }
        Ok(())
    }

    /// Wait up to `timeout` seconds for the reply to the last request.
    pub fn wait(&mut self, timeout: f64) -> Result<Reply, Lost> {
        match self
            .replies
            .recv_timeout(Duration::from_secs_f64(timeout.max(0.001)))
        {
            Ok(line) => serde_json::from_str(&line)
                .map_err(|e| Lost::Died(json!({"bad_reply": e.to_string(), "line": line}))),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Lost::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.died()),
        }
    }

    pub fn call(&mut self, request: &Request, timeout: f64) -> Result<Reply, Lost> {
        self.send(request)?;
        self.wait(timeout)
    }

    fn died(&mut self) -> Lost {
        let status = self.child.wait().ok();
        // Let the stderr reader finish the last lines.
        thread::sleep(Duration::from_millis(50));
        Lost::Died(self.exit_json(status))
    }

    fn exit_json(&self, status: Option<std::process::ExitStatus>) -> Value {
        use std::os::unix::process::ExitStatusExt;
        json!({"code": status.and_then(|s| s.code()), "signal": status.and_then(|s| s.signal()),
               "stderr": self.stderr.lock().unwrap().clone()})
    }

    /// Whether the worker has exited, and how.
    pub fn exited(&mut self) -> Option<Value> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                thread::sleep(Duration::from_millis(50));
                Some(self.exit_json(Some(status)))
            }
            _ => None,
        }
    }

    pub fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ------------------------------------------------------------ the run

/// What became of one transfer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ok,
    Failed(String),
    Unknown(String),
}

#[derive(Clone, Debug)]
struct Attempt {
    id: u128,
    fields: Fields,
    invoked: u64,
    completed: u64,
    outcome: Outcome,
    process: usize,
    at: f64,
    done_at: f64,
}

struct Shared {
    args: Args,
    addresses: String,
    seq: AtomicU64,
    next_id: AtomicU64,
    sessions: AtomicUsize,
    accounts: Vec<(u128, Fields)>,
    by_ledger: BTreeMap<u32, Vec<usize>>,
    zipf: Zipf,
    ledgers: Zipf,
    codes: Zipf,
    sparse: Zipf,
    user_data: Zipf,
    attempts: Mutex<Vec<Attempt>>,
    queries: Mutex<Vec<Query>>,
    counts: Mutex<BTreeMap<String, u64>>,
    /// The workload's end, set when the preload is done.
    stop_at: Mutex<Option<Instant>>,
    /// Oracle times of the fault schedule's steps.
    faults: Mutex<BTreeMap<String, f64>>,
    started: Mutex<Instant>,
    seed: u64,
}

impl Shared {
    fn tick(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    fn count(&self, key: &str) {
        *self.counts.lock().unwrap().entry(key.into()).or_default() += 1;
    }

    fn session(&self) -> Session {
        loop {
            let number = self.sessions.fetch_add(1, Ordering::SeqCst);
            match Session::spawn(number, &self.addresses, self.args.cluster, self.args.client) {
                Ok(session) => return session,
                Err(error) => {
                    evidence("worker-spawn-failed", json!({"error": error.0}));
                    kit::pause(1.0);
                }
            }
        }
    }

    /// The transfer user_data_64 epoch now (1-based).
    fn epoch(&self) -> u64 {
        1 + (self.started.lock().unwrap().elapsed().as_secs_f64() / self.args.epoch) as u64
    }

    fn pick_account(&self, rng: &mut Rng, ledger: u32) -> usize {
        let pool = &self.by_ledger[&ledger];
        loop {
            let index = self.zipf.sample(rng);
            if index < pool.len() {
                return pool[index];
            }
        }
    }

    /// Zipf-distributed field values: a few hot, most cool.
    fn random_fields(&self, rng: &mut Rng) -> Fields {
        Fields {
            ledger: self.ledgers.value(rng) as u32,
            code: self.codes.value(rng) as u16,
            user_data_32: self.user_data.value(rng) as u32,
            user_data_64: self.user_data.value(rng),
            user_data_128: 0,
            debit: 0,
            credit: 0,
        }
    }

    /// Whether the bulk preload is still running; ends it (and starts the
    /// workload's clock) once enough transfers were sent.
    fn preloading(&self) -> bool {
        let mut stop_at = self.stop_at.lock().unwrap();
        if stop_at.is_some() {
            return false;
        }
        if self.next_id.load(Ordering::SeqCst) > self.args.preload as u64 {
            *stop_at = Some(Instant::now() + Duration::from_secs_f64(self.args.duration));
            evidence(
                "preloaded",
                json!({"transfers": self.next_id.load(Ordering::SeqCst) - 1}),
            );
            return false;
        }
        true
    }

    fn done(&self) -> bool {
        self.stop_at
            .lock()
            .unwrap()
            .is_some_and(|at| Instant::now() >= at)
    }

    fn transfers(&self, rng: &mut Rng, batch: usize) -> Vec<Transfer> {
        let n = rng.range(1, batch as u64) as usize;
        let ledger = self.ledgers.value(rng) as u32;
        (0..n)
            .map(|_| {
                let debit = self.pick_account(rng, ledger);
                let mut credit = self.pick_account(rng, ledger);
                while credit == debit {
                    credit = self.pick_account(rng, ledger);
                }
                let mut f = self.random_fields(rng);
                f.ledger = ledger;
                if self.args.epoch > 0.0 {
                    f.user_data_64 = self.epoch();
                }
                f.user_data_32 = self.sparse.value(rng) as u32;
                if self.args.lean {
                    f.user_data_32 = 0;
                    f.user_data_64 = 0;
                }
                Transfer {
                    id: u128::from(self.next_id.fetch_add(1, Ordering::SeqCst)),
                    debit_account_id: self.accounts[debit].0,
                    credit_account_id: self.accounts[credit].0,
                    amount: if self.args.lean {
                        1
                    } else {
                        u128::from(rng.range(1, 100))
                    },
                    user_data_64: f.user_data_64,
                    user_data_32: f.user_data_32,
                    ledger,
                    code: f.code,
                    ..Transfer::default()
                }
            })
            .collect()
    }

    /// A random query of one to three fields (mostly two).
    fn query(&self, rng: &mut Rng) -> Request {
        let limit = *rng.choice(&[1u32, 3, 10, 50, 200, 1000, 8190]);
        let reversed = rng.chance(0.5);
        let width = *rng.choice(&[1usize, 2, 2, 2, 3]);
        let kind = rng.below(3);
        let mut f = self.random_fields(rng);
        if kind != 1 {
            if self.args.epoch > 0.0 {
                f.user_data_64 = rng.range(1, self.epoch());
            }
            f.user_data_32 = self.sparse.value(rng) as u32;
        }
        let mut filter = Filter::default();
        if kind == 2 {
            let ledger = self.ledgers.value(rng) as u32;
            let account = self.accounts[self.pick_account(rng, ledger)].0;
            filter.account = account;
            match rng.below(3) {
                0 => filter.debits = true,
                1 => filter.credits = true,
                _ => {
                    filter.debits = true;
                    filter.credits = true;
                }
            }
            let choices: &[usize] = if self.args.lean { &[0] } else { &[0, 1, 2] };
            let mut fields = rng.sample(choices, width.saturating_sub(1));
            fields.sort_unstable();
            for field in fields {
                match field {
                    0 => filter.code = f.code,
                    1 => filter.user_data_32 = f.user_data_32,
                    _ => filter.user_data_64 = f.user_data_64,
                }
            }
            return Request::GetAccountTransfers {
                selection: Selection {
                    filter,
                    limit,
                    reversed,
                },
            };
        }
        let choices: &[usize] = if self.args.lean && kind == 0 {
            &[0, 1]
        } else {
            &[0, 1, 2, 3]
        };
        for field in rng.sample(choices, width) {
            match field {
                0 => filter.ledger = f.ledger,
                1 => filter.code = f.code,
                2 => filter.user_data_32 = f.user_data_32,
                _ => filter.user_data_64 = f.user_data_64,
            }
        }
        let selection = Selection {
            filter,
            limit,
            reversed,
        };
        if kind == 0 {
            Request::QueryTransfers { selection }
        } else {
            Request::QueryAccounts { selection }
        }
    }

    fn record_query(&self, request: &Request, reply: &Reply, invoked: u64, completed: u64) {
        let (op, selection) = match request {
            Request::QueryTransfers { selection } => ("query_transfers", selection),
            Request::QueryAccounts { selection } => ("query_accounts", selection),
            Request::GetAccountTransfers { selection } => ("get_account_transfers", selection),
            _ => return,
        };
        let results = match reply {
            Reply::Transfers { transfers } => transfers.iter().map(returned_transfer).collect(),
            Reply::Accounts { accounts } => accounts.iter().map(returned_account).collect(),
            _ => return,
        };
        self.queries.lock().unwrap().push(Query {
            op: op.into(),
            filter: selection.filter,
            limit: selection.limit,
            reversed: selection.reversed,
            invoked,
            completed,
            results,
        });
    }

    /// One client process: a session issuing transfers and queries until the end.
    fn client(self: Arc<Self>, process: usize) {
        let mut rng = Rng::stream(self.seed, 100 + process as u64);
        let mut session = self.session();
        loop {
            if self.done() {
                break;
            }
            let request = if self.preloading() {
                let n = self.args.preload_batch;
                Request::CreateTransfers {
                    transfers: (0..n).flat_map(|_| self.transfers(&mut rng, 1)).collect(),
                }
            } else if rng.chance(self.args.query_fraction) {
                self.query(&mut rng)
            } else {
                Request::CreateTransfers {
                    transfers: self.transfers(&mut rng, self.args.batch),
                }
            };
            let invoked = self.tick();
            let at = kit::now();
            let result = session.call(&request, self.args.timeout);
            let completed = self.tick();
            let done_at = kit::now();
            self.count(&format!(
                "{}:{}",
                request.name(),
                match &result {
                    Ok(Reply::Status { status }) => status.as_str(),
                    Ok(_) => "ok",
                    Err(Lost::Timeout) => "timeout",
                    Err(Lost::Died(_)) => "died",
                }
            ));
            if let Request::CreateTransfers { transfers } = &request {
                let failures: BTreeMap<u32, u32> = match &result {
                    Ok(Reply::Created { results }) => {
                        results.iter().map(|r| (r.index, r.result)).collect()
                    }
                    _ => BTreeMap::new(),
                };
                let mut attempts = self.attempts.lock().unwrap();
                for (index, t) in transfers.iter().enumerate() {
                    let outcome = match &result {
                        Ok(Reply::Created { .. }) => match failures.get(&(index as u32)) {
                            None => Outcome::Ok,
                            Some(code) => {
                                Outcome::Failed(tb_client::create_result_name(true, *code))
                            }
                        },
                        Ok(Reply::Status { status }) => Outcome::Failed(status.clone()),
                        Ok(_) => Outcome::Unknown("unexpected reply".into()),
                        Err(lost) => Outcome::Unknown(lost.json().to_string()),
                    };
                    attempts.push(Attempt {
                        id: t.id,
                        fields: transfer_fields(t),
                        invoked,
                        completed,
                        outcome,
                        process,
                        at,
                        done_at,
                    });
                }
            } else if let Ok(reply) = &result {
                self.record_query(&request, reply, invoked, completed);
            }
            if let Err(lost) = result {
                evidence(
                    "info",
                    json!({"process": process, "session": session.number, "op": request.name(),
                           "lost": lost.json(), "since": at}),
                );
                session.kill();
                session = self.session();
            }
            if self.done() {
                break;
            }
            if self.args.think_ms > 0 {
                kit::pause(rng.below(self.args.think_ms + 1) as f64 / 1000.0);
            }
        }
        session.kill();
    }
}

fn transfer_fields(t: &Transfer) -> Fields {
    Fields {
        ledger: t.ledger,
        code: t.code,
        user_data_32: t.user_data_32,
        user_data_64: t.user_data_64,
        user_data_128: t.user_data_128,
        debit: t.debit_account_id,
        credit: t.credit_account_id,
    }
}

fn account_fields(a: &Account) -> Fields {
    Fields {
        ledger: a.ledger,
        code: a.code,
        user_data_32: a.user_data_32,
        user_data_64: a.user_data_64,
        user_data_128: a.user_data_128,
        debit: 0,
        credit: 0,
    }
}

fn returned_transfer(t: &Transfer) -> Returned {
    Returned {
        id: t.id,
        timestamp: t.timestamp,
        fields: transfer_fields(t),
    }
}

fn returned_account(a: &Account) -> Returned {
    Returned {
        id: a.id,
        timestamp: a.timestamp,
        fields: account_fields(a),
    }
}

/// Zipf-distributed indexes `0..n`: index i has weight 1 / (i + 1)^exponent.
#[derive(Clone, Debug)]
pub struct Zipf {
    cumulative: Vec<f64>,
}

impl Zipf {
    pub fn new(n: usize, exponent: f64) -> Self {
        let mut total = 0.0;
        let cumulative = (1..=n.max(1))
            .map(|i| {
                total += 1.0 / (i as f64).powf(exponent);
                total
            })
            .collect();
        Self { cumulative }
    }

    pub fn sample(&self, rng: &mut Rng) -> usize {
        let point = rng.unit() * self.cumulative.last().copied().unwrap_or(1.0);
        self.cumulative
            .partition_point(|c| *c <= point)
            .min(self.cumulative.len() - 1)
    }

    /// A value in `1..=n`.
    pub fn value(&self, rng: &mut Rng) -> u64 {
        self.sample(rng) as u64 + 1
    }
}

/// Send `line` to a node's agent, retrying until it answers.
fn agent(args: &Args, node: &str, line: &str, patience: f64) -> kit::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs_f64(patience);
    loop {
        match tigerbeetle_node::command(node, args.agent_port, line, Duration::from_secs(10)) {
            Ok(reply) => {
                evidence(
                    "agent",
                    json!({"node": node, "command": line, "reply": reply}),
                );
                return Ok(reply);
            }
            Err(error) if Instant::now() >= deadline => {
                return Err(kit::Error(format!("agent {node} {line}: {error}")))
            }
            Err(_) => kit::pause(0.5),
        }
    }
}

/// Call until a reply arrives, replacing the session after each loss.
fn call_until(
    shared: &Shared,
    session: &mut Session,
    request: &Request,
    patience: f64,
) -> kit::Result<Reply> {
    let deadline = Instant::now() + Duration::from_secs_f64(patience);
    loop {
        match session.call(request, shared.args.timeout) {
            Ok(reply) => return Ok(reply),
            Err(lost) => {
                evidence(
                    "setup-retry",
                    json!({"op": request.name(), "lost": lost.json(), "session": session.number}),
                );
                if let Lost::Died(exit) = &lost {
                    if exit["code"] == json!(2) {
                        crate::bail!("{}: the worker failed: {exit}", request.name());
                    }
                }
                kit::pause(1.0);
                let fresh = shared.session();
                std::mem::replace(session, fresh).kill();
                if Instant::now() >= deadline {
                    return Err(kit::Error(format!("{} never completed", request.name())));
                }
            }
        }
    }
}

fn setup(args: &Args) -> kit::Result<Arc<Shared>> {
    let addresses = match &args.addresses {
        Some(addresses) => addresses.clone(),
        None => {
            let mut out = Vec::new();
            for node in &args.nodes {
                out.push(tigerbeetle_node::resolve(
                    node,
                    args.port,
                    Duration::from_secs(120),
                )?);
            }
            out.join(",")
        }
    };
    let seed = args.seed.unwrap_or_else(kit::rng::entropy_seed);
    let mut rng = Rng::stream(seed, 1);
    let mut accounts = Vec::new();
    let mut by_ledger: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    let n = args.accounts.max(2 * args.ledgers as usize);
    for index in 0..n {
        let ledger = (index as u32 % args.ledgers) + 1;
        let fields = Fields {
            ledger,
            code: rng.range(1, u64::from(args.codes)) as u16,
            user_data_32: rng.range(1, u64::from(args.user_data)) as u32,
            user_data_64: rng.range(1, u64::from(args.user_data)),
            user_data_128: u128::from(rng.range(1, 1 << 40)),
            debit: 0,
            credit: 0,
        };
        by_ledger.entry(ledger).or_default().push(index);
        accounts.push((u128::from(1_000_000 + index as u64), fields));
    }
    let longest = by_ledger.values().map(Vec::len).max().unwrap_or(1);
    let shared = Arc::new(Shared {
        args: args.clone(),
        addresses,
        seq: AtomicU64::new(0),
        next_id: AtomicU64::new(1),
        sessions: AtomicUsize::new(0),
        accounts,
        by_ledger,
        zipf: Zipf::new(longest, args.zipf),
        ledgers: Zipf::new(args.ledgers as usize, args.zipf),
        codes: Zipf::new(args.codes as usize, args.zipf),
        sparse: Zipf::new(args.sparse as usize, args.zipf),
        user_data: Zipf::new(args.user_data as usize, args.zipf),
        attempts: Mutex::new(Vec::new()),
        queries: Mutex::new(Vec::new()),
        counts: Mutex::new(BTreeMap::new()),
        stop_at: Mutex::new(None),
        faults: Mutex::new(BTreeMap::new()),
        started: Mutex::new(Instant::now()),
        seed,
    });
    if args.agent_port != 0 {
        for node in &args.nodes {
            agent(args, node, "start", args.ready_timeout)?;
        }
    }
    let mut session = shared.session();
    let started = Instant::now();
    for chunk in shared.accounts.chunks(100) {
        let accounts: Vec<Account> = chunk
            .iter()
            .map(|(id, f)| Account {
                id: *id,
                user_data_128: f.user_data_128,
                user_data_64: f.user_data_64,
                user_data_32: f.user_data_32,
                ledger: f.ledger,
                code: f.code,
                ..Account::default()
            })
            .collect();
        let request = Request::CreateAccounts { accounts };
        match call_until(&shared, &mut session, &request, args.ready_timeout)? {
            Reply::Created { results } => {
                // `exists` is fine: an earlier, lost attempt created it.
                if let Some(bad) = results.iter().find(|r| r.result != 21) {
                    crate::bail!(
                        "create_accounts: {}",
                        tb_client::create_result_name(false, bad.result)
                    );
                }
            }
            other => crate::bail!("create_accounts: {other:?}"),
        }
    }
    session.kill();
    evidence(
        "ready",
        json!({"addresses": shared.addresses, "accounts": shared.accounts.len(),
               "seconds": started.elapsed().as_secs_f64(), "seed": seed,
               "client_release": shared.args.client.name()}),
    );
    Ok(shared)
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    evidence(
        "config",
        json!({"workload": format!("{:?}", args.workload).to_lowercase(), "nodes": args.nodes,
               "clients": args.clients, "duration": args.duration, "accounts": args.accounts,
               "ledgers": args.ledgers, "codes": args.codes, "user_data": args.user_data,
               "zipf": args.zipf, "batch": args.batch, "query_fraction": args.query_fraction,
               "timeout": args.timeout, "replace": format!("{:?}", args.replace).to_lowercase(),
               "cut_at": args.cut_at, "cut_for": args.cut_for, "alone_for": args.alone_for,
               "hold": args.hold, "sessions": args.sessions, "epoch": args.epoch, "sparse": args.sparse, "lean": args.lean, "preload": args.preload, "preload_batch": args.preload_batch,
               "max_pages": args.max_pages, "label": args.label}),
    );
    let shared = match setup(&args) {
        Ok(shared) => shared,
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("setup failed: {error}")),
    };
    match args.workload {
        Workload::Query => mixed(shared, false),
        Workload::Reformat => mixed(shared, true),
        Workload::Timeout => timeout(shared),
        Workload::Sessions => sessions(shared),
    }
}

// ------------------------------------------------------------ query and reformat

fn mixed(shared: Arc<Shared>, faults: bool) -> u8 {
    let args = shared.args.clone();
    let started = Instant::now();
    if args.preload == 0 {
        *shared.stop_at.lock().unwrap() = Some(started + Duration::from_secs_f64(args.duration));
    }
    *shared.started.lock().unwrap() = started;
    let nemesis = faults.then(|| {
        let shared = shared.clone();
        thread::spawn(move || replace_schedule(&shared, started))
    });
    let threads: Vec<_> = (0..args.clients)
        .map(|process| {
            let shared = shared.clone();
            thread::spawn(move || shared.client(process))
        })
        .collect();
    let mut last = Instant::now();
    while threads.iter().any(|t| !t.is_finished()) {
        kit::pause(1.0);
        if last.elapsed() >= Duration::from_secs(10) {
            last = Instant::now();
            evidence(
                "progress",
                json!({"counts": *shared.counts.lock().unwrap()}),
            );
        }
    }
    for thread in threads {
        let _ = thread.join();
    }
    let nemesis_ok = match nemesis.map(|t| t.join()) {
        Some(Ok(Ok(()))) | None => true,
        Some(Ok(Err(error))) => {
            evidence("nemesis-failed", json!({"error": error.0}));
            false
        }
        Some(Err(_)) => false,
    };
    evidence(
        "workload-done",
        json!({"counts": *shared.counts.lock().unwrap(), "seconds": started.elapsed().as_secs_f64()}),
    );
    if faults {
        if let Err(error) = final_heal(&shared) {
            return verdict(kit::INCONCLUSIVE, &format!("final heal failed: {error}"));
        }
    }
    let (transfers, accounts) = match final_reads(&shared) {
        Ok(models) => models,
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("final read failed: {error}")),
    };
    let durability = durability(&shared, &transfers);
    let lost = durability["lost"].as_u64().unwrap_or(0);
    let phantom = durability["failed_but_present"].as_u64().unwrap_or(0);
    let ok_transfers = durability["acknowledged"].as_u64().unwrap_or(0);
    let mut problems = Vec::new();
    if lost > 0 {
        problems.push(format!("{lost} acknowledged transfers lost"));
    }
    if phantom > 0 {
        problems.push(format!("{phantom} failed transfers present"));
    }
    if !faults {
        let (live, battery) = check_queries(&shared, &transfers, &accounts);
        let missing_live = live.violations;
        let missing_final = battery.violations;
        if missing_live + missing_final > 0 {
            problems.push(format!(
                "{} of {} queries during the run and {} of {} final queries returned wrong results ({} objects missing)",
                missing_live, live.queries, missing_final, battery.queries,
                live.missing_objects + battery.missing_objects
            ));
        }
        if live.queries + battery.queries == 0 {
            return verdict(kit::INCONCLUSIVE, "no query completed");
        }
    }
    if ok_transfers == 0 {
        return verdict(kit::INCONCLUSIVE, "no transfer was acknowledged");
    }
    if !problems.is_empty() {
        return verdict(kit::VIOLATION, &problems.join("; "));
    }
    if faults && !nemesis_ok {
        return verdict(kit::INCONCLUSIVE, "the fault schedule did not complete");
    }
    verdict(
        kit::OK,
        &format!(
            "{ok_transfers} acknowledged transfers all present; every checked query result correct"
        ),
    )
}

/// tigerbeetle#2767's schedule: cut one replica off, kill the two that hold
/// the acknowledged transfers, replace one of them and let it run with the
/// cut-off replica before the other returns.
fn replace_schedule(shared: &Shared, started: Instant) -> kit::Result<()> {
    let args = &shared.args;
    let mut ctl = kit::connect_world(None)?;
    let mut rng = Rng::stream(shared.seed, 7);
    let nodes = &args.nodes;
    let behind = args
        .cut_node
        .unwrap_or_else(|| rng.below(nodes.len() as u64) as usize);
    let others: Vec<usize> = (0..nodes.len()).filter(|i| *i != behind).collect();
    let replaced = args
        .replace_node
        .filter(|i| others.contains(i))
        .unwrap_or_else(|| others[rng.below(others.len() as u64) as usize]);
    let restarted: Vec<usize> = others.iter().copied().filter(|i| *i != replaced).collect();
    let at = |seconds: f64| kit::pause(seconds - started.elapsed().as_secs_f64());
    let fault = |action: &str, targets: Vec<&String>| {
        shared
            .faults
            .lock()
            .unwrap()
            .insert(action.into(), kit::now());
        evidence("fault", json!({"action": action, "targets": targets}));
    };
    at(args.cut_at);
    let rest: Vec<String> = others.iter().map(|i| nodes[*i].clone()).collect();
    let cut = ctl.partition(&[nodes[behind].clone()], &rest)?;
    fault("partition", vec![&nodes[behind]]);
    at(args.cut_at + args.cut_for);
    for i in &others {
        ctl.stop(&nodes[*i])?;
    }
    fault("stop", others.iter().map(|i| &nodes[*i]).collect());
    ctl.heal(&cut)?;
    fault("heal", vec![&nodes[behind]]);
    ctl.restart(&nodes[replaced])?;
    let line = match args.replace {
        Replace::Format => "reformat",
        Replace::Recover => "recover",
    };
    agent(args, &nodes[replaced], line, 60.0)?;
    fault(line, vec![&nodes[replaced]]);
    at(args.cut_at + args.cut_for + args.alone_for);
    for i in &restarted {
        ctl.restart(&nodes[*i])?;
        agent(args, &nodes[*i], "start", 60.0)?;
    }
    fault("restart", restarted.iter().map(|i| &nodes[*i]).collect());
    Ok(())
}

fn final_heal(shared: &Shared) -> kit::Result<()> {
    let mut ctl = kit::connect_world(None)?;
    let failures = ctl.heal_everything();
    if !failures.is_empty() {
        crate::bail!("{}", failures.join("; "));
    }
    for node in &shared.args.nodes {
        let reply = agent(&shared.args, node, "status", 60.0)?;
        if reply["running"] != json!(true) && reply["recovering"] != json!(true) {
            agent(&shared.args, node, "start", 60.0)?;
        }
    }
    Ok(())
}

/// Read every attempted transfer and every account back, once the cluster serves.
fn final_reads(shared: &Shared) -> kit::Result<(Model, Model)> {
    let started = Instant::now();
    let mut session = shared.session();
    let attempts = shared.attempts.lock().unwrap().clone();
    let mut transfers = Model::default();
    let ids: Vec<u128> = attempts.iter().map(|a| a.id).collect();
    let index: BTreeMap<u128, &Attempt> = attempts.iter().map(|a| (a.id, a)).collect();
    for chunk in ids.chunks(shared.args.lookup_batch) {
        let request = Request::LookupTransfers {
            ids: chunk.to_vec(),
        };
        match call_until(shared, &mut session, &request, shared.args.ready_timeout)? {
            Reply::Transfers { transfers: found } => {
                for t in found {
                    let a = index[&t.id];
                    transfers.insert(Object {
                        id: t.id,
                        timestamp: t.timestamp,
                        fields: transfer_fields(&t),
                        acked: if a.outcome == Outcome::Ok {
                            a.completed
                        } else {
                            u64::MAX
                        },
                        invoked: a.invoked,
                    });
                    if transfer_fields(&t) != a.fields {
                        evidence(
                            "transfer-differs",
                            json!({"id": t.id, "read": transfer_fields(&t), "written": a.fields}),
                        );
                    }
                }
            }
            other => crate::bail!("lookup_transfers: {other:?}"),
        }
    }
    let mut accounts = Model::default();
    let ids: Vec<u128> = shared.accounts.iter().map(|(id, _)| *id).collect();
    for chunk in ids.chunks(shared.args.lookup_batch) {
        let request = Request::LookupAccounts {
            ids: chunk.to_vec(),
        };
        match call_until(shared, &mut session, &request, shared.args.ready_timeout)? {
            Reply::Accounts { accounts: found } => {
                for a in found {
                    accounts.insert(Object {
                        id: a.id,
                        timestamp: a.timestamp,
                        fields: account_fields(&a),
                        acked: 0,
                        invoked: 0,
                    });
                }
            }
            other => crate::bail!("lookup_accounts: {other:?}"),
        }
    }
    session.kill();
    evidence(
        "final-read",
        json!({"transfers_found": transfers.objects.len(), "attempted": attempts.len(),
               "accounts_found": accounts.objects.len(), "seconds": started.elapsed().as_secs_f64()}),
    );
    if accounts.objects.len() != shared.accounts.len() {
        evidence(
            "accounts-missing",
            json!({"found": accounts.objects.len(), "created": shared.accounts.len()}),
        );
    }
    Ok((transfers, accounts))
}

fn durability(shared: &Shared, transfers: &Model) -> Value {
    let attempts = shared.attempts.lock().unwrap();
    let mut lost = Vec::new();
    let mut phantom = Vec::new();
    let mut unknown_present = 0;
    let mut unknown = 0;
    let mut acked = 0;
    let mut by_process: BTreeMap<usize, usize> = BTreeMap::new();
    for a in attempts.iter() {
        let present = transfers.objects.contains_key(&a.id);
        match &a.outcome {
            Outcome::Ok => {
                acked += 1;
                if !present {
                    *by_process.entry(a.process).or_default() += 1;
                    lost.push(a);
                }
            }
            Outcome::Failed(why) => {
                // Only a definite rejection must leave nothing behind.
                if present {
                    phantom.push(json!({"id": a.id, "why": why, "at": a.at}));
                }
            }
            Outcome::Unknown(_) => {
                unknown += 1;
                if present {
                    unknown_present += 1;
                }
            }
        }
    }
    let examples: Vec<Value> = lost
        .iter()
        .take(shared.args.examples * 4)
        .map(|a| json!({"id": a.id, "process": a.process, "invoked_at": a.at}))
        .collect();
    let lost_times: Vec<f64> = lost.iter().map(|a| a.at).collect();
    // Transfers acknowledged while one replica was cut off: only the two
    // replicas that were then killed held them.
    let faults = shared.faults.lock().unwrap().clone();
    let exposed = match (faults.get("partition"), faults.get("stop")) {
        (Some(cut), Some(stop)) => attempts
            .iter()
            .filter(|a| a.outcome == Outcome::Ok && a.done_at >= *cut && a.done_at <= *stop)
            .count(),
        _ => 0,
    };
    let value = json!({"acknowledged": acked, "lost": lost.len(), "lost_examples": examples,
        "lost_invoked_first": lost_times.iter().cloned().fold(f64::INFINITY, f64::min),
        "lost_invoked_last": lost_times.iter().cloned().fold(0.0, f64::max),
        "lost_by_process": by_process, "acked_while_cut": exposed, "faults": faults,
        "failed_but_present": phantom.len(), "failed_but_present_examples": phantom.iter().take(shared.args.examples).collect::<Vec<_>>(),
        "indeterminate": unknown, "indeterminate_present": unknown_present});
    evidence("durability", value.clone());
    value
}

fn check_queries(
    shared: &Shared,
    transfers: &Model,
    accounts: &Model,
) -> (ledger::Summary, ledger::Summary) {
    let queries = shared.queries.lock().unwrap().clone();
    let mut live = ledger::Summary::default();
    let mut shown = 0;
    for q in &queries {
        let model = if q.op == "query_accounts" {
            accounts
        } else {
            transfers
        };
        let violation = model.check(q);
        if let Some(v) = &violation {
            if shown < shared.args.examples {
                shown += 1;
                evidence("query-violation", json!({"phase": "live", "violation": v}));
            }
        }
        live.add(q, violation.as_ref());
    }
    evidence("queries-live", json!(live));
    let battery = final_queries(shared, transfers, accounts);
    evidence("queries-final", json!(battery));
    (live, battery)
}

/// Page through every filter of one and two fields (and the busiest
/// accounts' transfers) after the workload, checking each page exactly.
fn final_queries(shared: &Shared, transfers: &Model, accounts: &Model) -> ledger::Summary {
    let args = &shared.args;
    let mut rng = Rng::stream(shared.seed, 9);
    let mut summary = ledger::Summary::default();
    let mut session = shared.session();
    let mut filters: Vec<(&str, Filter)> = Vec::new();
    let ud = u64::from(args.user_data);
    let epochs = if args.epoch > 0.0 {
        transfers
            .objects
            .values()
            .map(|o| o.fields.user_data_64)
            .max()
            .unwrap_or(1)
    } else {
        ud
    };
    let set = |filter: &mut Filter, field: &str, value: u64| match field {
        "ledger" => filter.ledger = value as u32,
        "code" => filter.code = value as u16,
        "ud32" => filter.user_data_32 = value as u32,
        _ => filter.user_data_64 = value,
    };
    let sparse = u64::from(args.sparse);
    for (op, ud32, ud64) in [
        ("query_transfers", sparse, epochs),
        ("query_accounts", ud, ud),
    ] {
        let mut fields: Vec<(&str, u64)> = vec![
            ("ledger", u64::from(args.ledgers)),
            ("code", u64::from(args.codes)),
            ("ud32", ud32),
            ("ud64", ud64),
        ];
        if args.lean && op == "query_transfers" {
            fields.truncate(2);
        }
        for (i, (a, an)) in fields.iter().enumerate() {
            for av in 1..=*an {
                let mut f = Filter::default();
                set(&mut f, a, av);
                filters.push((op, f));
                for (b, bn) in &fields[i + 1..] {
                    for bv in 1..=*bn {
                        let mut g = f;
                        set(&mut g, b, bv);
                        filters.push((op, g));
                    }
                }
            }
        }
    }
    rng.shuffle(&mut filters);
    filters.truncate(args.final_filters);
    // The busiest accounts of each ledger, with and without a code.
    for pool in shared.by_ledger.values() {
        for index in pool.iter().take(3) {
            let account = shared.accounts[*index].0;
            for code in 0..=u64::from(args.codes) {
                filters.push((
                    "get_account_transfers",
                    Filter {
                        account,
                        debits: true,
                        credits: true,
                        code: code as u16,
                        ..Filter::default()
                    },
                ));
            }
        }
    }
    let mut shown = 0;
    'filters: for (op, filter) in filters {
        let page = args
            .page
            .unwrap_or_else(|| *rng.choice(&[50u32, 500, 8190]));
        for reversed in [false, true] {
            let mut bound = 0u64;
            for _ in 0..args.max_pages {
                let mut f = filter;
                if reversed {
                    f.timestamp_max = bound;
                } else {
                    f.timestamp_min = bound;
                }
                let selection = Selection {
                    filter: f,
                    limit: page,
                    reversed,
                };
                let request = match op {
                    "query_transfers" => Request::QueryTransfers { selection },
                    "query_accounts" => Request::QueryAccounts { selection },
                    _ => Request::GetAccountTransfers { selection },
                };
                let invoked = shared.tick();
                let reply = match session.call(&request, args.timeout) {
                    Ok(reply) => reply,
                    Err(lost) => {
                        evidence("final-query-lost", json!({"lost": lost.json()}));
                        session.kill();
                        session = shared.session();
                        continue 'filters;
                    }
                };
                let completed = shared.tick();
                let results: Vec<Returned> = match &reply {
                    Reply::Transfers { transfers } => {
                        transfers.iter().map(returned_transfer).collect()
                    }
                    Reply::Accounts { accounts } => accounts.iter().map(returned_account).collect(),
                    other => {
                        evidence("final-query-failed", json!({"reply": format!("{other:?}")}));
                        continue 'filters;
                    }
                };
                let next = results.last().map(|r| {
                    if reversed {
                        r.timestamp.saturating_sub(1)
                    } else {
                        r.timestamp + 1
                    }
                });
                let full = results.len() == page as usize;
                let query = Query {
                    op: op.into(),
                    filter: f,
                    limit: page,
                    reversed,
                    invoked,
                    completed,
                    results,
                };
                let model = if op == "query_accounts" {
                    accounts
                } else {
                    transfers
                };
                let violation = model.check(&query);
                if let Some(v) = &violation {
                    if shown < args.examples {
                        shown += 1;
                        evidence("query-violation", json!({"phase": "final", "violation": v}));
                    }
                }
                summary.add(&query, violation.as_ref());
                match next {
                    Some(next) if full && next != 0 => bound = next,
                    _ => break,
                }
            }
        }
    }
    session.kill();
    summary
}

// ------------------------------------------------------------ timeout

fn timeout(shared: Arc<Shared>) -> u8 {
    let args = &shared.args;
    let mut ctl = match kit::connect_world(None) {
        Ok(ctl) => ctl,
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("world: {error}")),
    };
    let mut warm = shared.session();
    let probe = Request::LookupAccounts {
        ids: vec![shared.accounts[0].0],
    };
    if let Err(error) = call_until(&shared, &mut warm, &probe, args.ready_timeout) {
        return verdict(kit::INCONCLUSIVE, &format!("cluster not serving: {error}"));
    }
    for node in &args.nodes {
        if let Err(error) = ctl.stop(node) {
            return verdict(kit::INCONCLUSIVE, &format!("stop {node}: {error}"));
        }
    }
    evidence("fault", json!({"action": "stop", "targets": args.nodes}));
    let transfer = Transfer {
        id: 42,
        debit_account_id: shared.accounts[0].0,
        credit_account_id: shared.accounts[shared.by_ledger[&1][1]].0,
        amount: 1,
        ledger: 1,
        code: 1,
        ..Transfer::default()
    };
    let submitted = Instant::now();
    let request = Request::CreateTransfers {
        transfers: vec![transfer],
    };
    // An existing session, and a new one that must register first.
    let mut fresh = shared.session();
    let sent = warm.send(&request);
    let fresh_sent = fresh.send(&Request::LookupAccounts {
        ids: vec![shared.accounts[0].0],
    });
    let mut during = BTreeMap::new();
    let mut lost_during = None;
    let mut fresh_during = None;
    for checkpoint in [1.0, 10.0, args.hold] {
        let waited = submitted.elapsed().as_secs_f64();
        if lost_during.is_none() && sent.is_ok() {
            match warm.wait(checkpoint - waited) {
                Ok(reply) => {
                    lost_during = Some(
                        json!({"reply": format!("{reply:?}"), "after": submitted.elapsed().as_secs_f64()}),
                    )
                }
                Err(Lost::Died(exit)) => lost_during = Some(json!({"died": exit})),
                Err(Lost::Timeout) => {}
            }
        }
        if fresh_during.is_none() && fresh_sent.is_ok() {
            match fresh.wait(0.001) {
                Ok(reply) => fresh_during = Some(json!({"reply": format!("{reply:?}")})),
                Err(Lost::Died(exit)) => fresh_during = Some(json!({"died": exit})),
                Err(Lost::Timeout) => {}
            }
        }
        during.insert(
            format!("{checkpoint}"),
            json!({"request": lost_during.clone().unwrap_or(json!("pending")),
                   "new_session": fresh_during.clone().unwrap_or(json!("pending"))}),
        );
        evidence(
            "outage",
            json!({"after": submitted.elapsed().as_secs_f64(), "request": lost_during, "new_session": fresh_during}),
        );
    }
    let pending_through_outage = lost_during.is_none();
    for node in &args.nodes {
        if let Err(error) = ctl.restart(node).map_err(|e| e.0).and_then(|()| {
            agent(args, node, "start", 60.0)
                .map(|_| ())
                .map_err(|e| e.0)
        }) {
            return verdict(kit::INCONCLUSIVE, &format!("restart {node}: {error}"));
        }
    }
    evidence("fault", json!({"action": "restart", "targets": args.nodes}));
    let restarted = Instant::now();
    let after = if pending_through_outage {
        match warm.wait(args.ready_timeout) {
            Ok(reply) => {
                json!({"reply": format!("{reply:?}"), "after_restart": restarted.elapsed().as_secs_f64(),
                                "after_submit": submitted.elapsed().as_secs_f64()})
            }
            Err(lost) => json!({"lost": lost.json()}),
        }
    } else {
        json!("completed during the outage")
    };
    let fresh_after = if fresh_during.is_none() {
        match fresh.wait(args.ready_timeout) {
            Ok(reply) => {
                json!({"reply": format!("{reply:?}"), "after_restart": restarted.elapsed().as_secs_f64()})
            }
            Err(lost) => json!({"lost": lost.json()}),
        }
    } else {
        json!("completed during the outage")
    };
    let mut check = shared.session();
    let present = match call_until(
        &shared,
        &mut check,
        &Request::LookupTransfers { ids: vec![42] },
        args.ready_timeout,
    ) {
        Ok(Reply::Transfers { transfers }) => transfers.len() == 1,
        _ => false,
    };
    evidence(
        "timeout",
        json!({"hold": args.hold, "pending_through_outage": pending_through_outage, "during": during,
               "after": after, "new_session_after": fresh_after, "transfer_present": present}),
    );
    warm.kill();
    fresh.kill();
    check.kill();
    if pending_through_outage {
        verdict(
            kit::VIOLATION,
            &format!(
                "a create_transfers request to a cluster with every replica down neither failed nor timed out in {} s; {} after the restart",
                args.hold,
                if present { "it then committed" } else { "it never committed" }
            ),
        )
    } else {
        verdict(kit::OK, "the request completed or failed during the outage")
    }
}

// ------------------------------------------------------------ sessions

/// tigerbeetle#2484. A replica keeps at most 64 client connections (the
/// 8-prepare pipeline plus 56 queued requests) and 64 sessions, evicting the
/// least recently registered session when a 65th registers. Open 64 sessions,
/// then `--evictions` times: close the newest (freeing a connection, not its
/// session) and open a new one, whose registration evicts the oldest live
/// session. A 0.16.11 client panics on eviction; 0.16.13 and later fail the
/// client's requests instead.
fn sessions(shared: Arc<Shared>) -> u8 {
    let args = &shared.args;
    let probe = Request::LookupAccounts {
        ids: vec![shared.accounts[0].0],
    };
    let mut open: Vec<Session> = Vec::new();
    let mut first_failures = 0;
    let mut deaths: BTreeMap<usize, Value> = BTreeMap::new();
    let mut evictions = Vec::new();
    let reap = |open: &mut Vec<Session>, deaths: &mut BTreeMap<usize, Value>, phase: &str| {
        for s in open.iter_mut() {
            if !deaths.contains_key(&s.number) {
                if let Some(exit) = s.exited() {
                    evidence(
                        "session-died",
                        json!({"session": s.number, "phase": phase, "exit": exit}),
                    );
                    deaths.insert(s.number, exit);
                }
            }
        }
    };
    for _ in 0..args.sessions {
        let mut session = shared.session();
        if session.call(&probe, args.timeout).is_err() {
            first_failures += 1;
        }
        open.push(session);
    }
    reap(&mut open, &mut deaths, "open");
    evidence(
        "sessions-open",
        json!({"open": open.len(), "first_failures": first_failures, "died": deaths.len()}),
    );
    for round in 0..args.evictions {
        if let Some(newest) = open.pop() {
            newest.kill();
        }
        let oldest_live = open
            .iter()
            .map(|s| s.number)
            .find(|n| !deaths.contains_key(n));
        let mut session = shared.session();
        let registered = session.call(&probe, args.timeout);
        open.push(session);
        kit::pause(1.0);
        reap(&mut open, &mut deaths, "evict");
        evictions.push(
            json!({"round": round, "registered": registered.is_ok(), "oldest_live": oldest_live,
                              "oldest_died": oldest_live.is_some_and(|n| deaths.contains_key(&n))}),
        );
    }
    // Use every surviving session once more.
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    for s in open.iter_mut() {
        if deaths.contains_key(&s.number) {
            continue;
        }
        let key = match s.call(&probe, 10.0) {
            Ok(Reply::Status { status }) => status,
            Ok(_) => "ok".into(),
            Err(Lost::Timeout) => "timeout".into(),
            Err(Lost::Died(exit)) => {
                evidence(
                    "session-died",
                    json!({"session": s.number, "phase": "reuse", "exit": exit}),
                );
                deaths.insert(s.number, exit);
                "died".into()
            }
        };
        *statuses.entry(key).or_default() += 1;
    }
    let panicked: Vec<usize> = deaths
        .iter()
        .filter(|(_, exit)| exit.to_string().contains("evicted"))
        .map(|(n, _)| *n)
        .collect();
    evidence(
        "sessions",
        json!({"opened": args.sessions, "evictions": evictions, "died": deaths.len(),
               "panicked_on_eviction": panicked, "reuse": statuses}),
    );
    for s in open {
        s.kill();
    }
    if !panicked.is_empty() {
        verdict(
            kit::VIOLATION,
            &format!(
                "{} client processes crashed (panic: session evicted) when newer sessions evicted theirs",
                panicked.len()
            ),
        )
    } else if deaths.is_empty() && !statuses.keys().any(|k| k.contains("evict")) {
        verdict(kit::INCONCLUSIVE, "no session was evicted")
    } else {
        verdict(
            kit::OK,
            "evicted sessions failed their requests without crashing",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_round_trip_as_json() {
        let request = Request::QueryTransfers {
            selection: Selection {
                filter: Filter {
                    ledger: 1,
                    code: 2,
                    ..Filter::default()
                },
                limit: 10,
                reversed: true,
            },
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.starts_with("{\"query_transfers\":"), "{text}");
        let back: Request = serde_json::from_str(&text).unwrap();
        let Request::QueryTransfers { selection } = back else {
            panic!()
        };
        let filter = query_filter(&selection);
        assert_eq!(
            (filter.ledger, filter.code, filter.limit, filter.flags),
            (1, 2, 10, 1)
        );
        let reply = Reply::Transfers {
            transfers: vec![Transfer {
                id: u128::MAX - 1,
                ..Transfer::default()
            }],
        };
        let back: Reply = serde_json::from_str(&serde_json::to_string(&reply).unwrap()).unwrap();
        let Reply::Transfers { transfers } = back else {
            panic!()
        };
        assert_eq!(transfers[0].id, u128::MAX - 1);
    }

    #[test]
    fn account_filters_carry_sides_and_direction() {
        let s = Selection {
            filter: Filter {
                account: 9,
                credits: true,
                code: 3,
                ..Filter::default()
            },
            limit: 5,
            reversed: true,
        };
        let f = account_filter(&s);
        assert_eq!((f.account_id, f.code, f.flags), (9, 3, 2 | 4));
    }
}
