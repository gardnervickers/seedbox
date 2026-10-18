//! One TigerBeetle replica's machine: a small agent that owns the replica's
//! data file and process, and takes commands from the oracle, as
//! `galera-node.sh` and Kafka's `start-broker.sh` do for theirs.
//!
//! On every boot (the first, and each `restart` after a `stop` or `reboot`
//! after a `crash`) the agent formats the data file if there is none, then
//! listens on `--agent-port` for one-line commands, each answered with one
//! JSON line:
//!
//! * `status`: whether the replica runs, its release, past exits (each with
//!   the last lines it wrote), notable log lines, the data file's size.
//! * `start`: start `tigerbeetle start` on the existing data file.
//! * `reformat`: delete the data file, `tigerbeetle format` a new one, start.
//!   This is the undocumented node-replacement procedure of tigerbeetle#2767.
//! * `recover`: delete the data file and run `tigerbeetle recover` (0.16.43 and
//!   later), which waits for the cluster, then start.
//! * `kill`: SIGKILL the replica process but keep the agent.
//! * `install RELEASE`: atomically replace the replica's binary with
//!   `--releases/RELEASE/tigerbeetle` (needs `--binary-dir`). A production
//!   replica polls its binary every second and, once every replica has the
//!   new release, the cluster upgrades itself and each replica `exec`s it.
//!   `install RELEASE SECONDS` does so that many seconds later: if the machine
//!   is paused meanwhile, as soon as it resumes.
//!
//! The replica's stdout and stderr pass through the agent, which copies each
//! line to its own stderr and keeps the notable ones (upgrades, state sync,
//! panics) for `status`. Lines the agent writes itself start with `TBNODE`.
//! Addresses are resolved to IPv4 here, because TigerBeetle 0.16 accepts only
//! IP addresses. With `--respawn N` an unexpected exit is followed by a new
//! `start` a second later, up to N times, as a service manager would. With
//! `--start-on-reboot` a boot that finds an existing data file (the machine
//! rebooted after a crash) starts the replica at once, so generated crashes
//! need no oracle step to bring it back.
//!
//! `--probe DIR` first checks that `DIR`'s file system takes O_DIRECT reads
//! and writes through io_uring (TigerBeetle's I/O path) and `fallocate`, and
//! reports the result as `TBNODE {"io_probe": ...}`.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// This replica's index.
    #[arg(long)]
    pub replica: u8,
    /// Every replica's machine, in replica order.
    #[arg(long, value_delimiter = ',')]
    pub peers: Vec<String>,
    #[arg(long, default_value_t = 3000)]
    pub port: u16,
    #[arg(long, default_value_t = 0)]
    pub cluster: u128,
    /// The replica's data file.
    #[arg(long)]
    pub data: PathBuf,
    #[arg(long, default_value = "/tigerbeetle")]
    pub tigerbeetle: PathBuf,
    /// Run the replica from `DIR/tigerbeetle`, installed on first boot from
    /// --release (or --tigerbeetle) and replaced by `install`; keep it on the
    /// data volume so it outlives a crash.
    #[arg(long)]
    pub binary_dir: Option<PathBuf>,
    /// Where `install RELEASE` finds `RELEASE/tigerbeetle`.
    #[arg(long, default_value = "/opt/tigerbeetle")]
    pub releases: PathBuf,
    /// The release --binary-dir starts with [default: --tigerbeetle].
    #[arg(long)]
    pub release: Option<String>,
    /// Start the replica again after this many unexpected exits.
    #[arg(long, default_value_t = 0)]
    pub respawn: u32,
    /// On a boot that finds an existing data file, start the replica at once.
    #[arg(long)]
    pub start_on_reboot: bool,
    /// Extra `tigerbeetle format` argument; repeatable.
    #[arg(long = "format-arg", allow_hyphen_values = true)]
    pub format_args: Vec<String>,
    /// Extra `tigerbeetle start` argument; repeatable.
    #[arg(long = "start-arg", allow_hyphen_values = true)]
    pub start_args: Vec<String>,
    #[arg(long, default_value_t = 4599)]
    pub agent_port: u16,
    /// Mount a ramfs here first (unless one is there), for a data file in
    /// memory that, unlike tmpfs, `fallocate` does not fill: the file stays
    /// sparse and only what TigerBeetle writes takes memory.
    #[arg(long)]
    pub ramfs: Option<PathBuf>,
    /// Check O_DIRECT, io_uring and fallocate in this directory first; repeatable.
    #[arg(long)]
    pub probe: Vec<PathBuf>,
}

/// One `TBNODE` line in a single write, so it never interleaves with the
/// replica's own output on the shared stderr.
fn say(value: Value) {
    let _ = std::io::stderr().write_all(format!("TBNODE {value}\n").as_bytes());
}

/// `name:port` as `ip:port`, waiting until the name resolves.
pub fn resolve(name: &str, port: u16, patience: Duration) -> Result<String, String> {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(addresses) = (name, port).to_socket_addrs() {
            if let Some(address) = addresses.into_iter().find(|a| a.is_ipv4()) {
                return Ok(address.to_string());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("{name}: does not resolve"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// What the replica's output has shown so far.
#[derive(Default)]
struct Output {
    /// The release the running process reported at startup.
    release: Option<String>,
    /// Notable lines, oldest first (at most [`NOTABLE_MAX`]).
    notable: Vec<Value>,
    /// The last lines of any kind, for an exit's report.
    tail: Vec<String>,
    /// Lines that mention a panic.
    panics: u32,
    /// The view and role of the last transition to normal status.
    view: Option<u64>,
    role: Option<String>,
}

const NOTABLE_MAX: usize = 400;
const TAIL_MAX: usize = 30;

/// Lines worth keeping: release changes, upgrades, state sync, failures.
fn notable(line: &str) -> bool {
    if line.contains("(message_bus)") || line.contains("(clock)") {
        return false;
    }
    [
        "panic",
        "release_transition",
        "executing",
        "upgrading from",
        "diverged",
        "sync_",
        "state sync",
        "unreachable",
        "corrupt",
        "binary change",
        "(main): release=",
        "releases_bundled=",
        "error(",
        "(multiversioning)",
        "fatal",
    ]
    .iter()
    .any(|needle| line.contains(needle))
}

/// Copy a child's output to our stderr line by line, recording what matters.
fn pass_through(pipe: impl Read + Send + 'static, output: Arc<Mutex<Output>>, started: Instant) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if !bytes.ends_with(b"\n") {
                bytes.push(b'\n');
            }
            let _ = std::io::stderr().write_all(&bytes);
            let line = String::from_utf8_lossy(&bytes).trim_end().to_string();
            let mut out = output.lock().unwrap();
            // Release, role and upgrades are guest state too, so a campaign
            // can place faults relative to them.
            if let Some(at) = line.find("(main): release=") {
                let release = line[at + "(main): release=".len()..].trim();
                seedbox_sdk::state("release", release);
                out.release = Some(release.to_string());
            }
            if let Some((view, role)) = normal_view(&line) {
                seedbox_sdk::state("role", &role);
                out.view = Some(view);
                out.role = Some(role);
            }
            if line.contains("upgrading from") {
                seedbox_sdk::event("upgrading", serde_json::Value::Null);
            }
            if line.contains("panic") {
                out.panics += 1;
            }
            if notable(&line) {
                if out.notable.len() >= NOTABLE_MAX {
                    out.notable.remove(0);
                }
                let at = started.elapsed().as_secs_f64();
                out.notable
                    .push(json!({"at": (at * 1000.0).round() / 1000.0, "line": line}));
            }
            out.tail.push(line);
            if out.tail.len() > TAIL_MAX {
                out.tail.remove(0);
            }
        }
    });
}

struct Agent {
    args: Args,
    addresses: String,
    replica: Option<Child>,
    recovering: Option<Child>,
    exits: Vec<Value>,
    starts: u32,
    output: Arc<Mutex<Output>>,
    /// The agent's start, the clock of notable lines.
    booted: Instant,
    /// The oracle killed the replica: its exit is expected.
    killed: bool,
    respawns: u32,
    respawn_at: Option<Instant>,
    installs: Vec<Value>,
    /// Installs due later: (when, release).
    pending: Vec<(Instant, String)>,
}

impl Agent {
    fn binary(&self) -> PathBuf {
        match &self.args.binary_dir {
            Some(dir) => dir.join("tigerbeetle"),
            None => self.args.tigerbeetle.clone(),
        }
    }

    fn tigerbeetle(&self) -> Command {
        Command::new(self.binary())
    }

    /// Spawn `command` with its output passed through.
    fn spawn(&self, mut command: Command) -> std::io::Result<Child> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(pipe) = child.stdout.take() {
            pass_through(pipe, self.output.clone(), self.booted);
        }
        if let Some(pipe) = child.stderr.take() {
            pass_through(pipe, self.output.clone(), self.booted);
        }
        Ok(child)
    }

    /// Replace the replica's binary with `release`'s, atomically and durably.
    fn install(&mut self, release: &str) -> Result<Value, String> {
        let Some(dir) = self.args.binary_dir.clone() else {
            return Err("no --binary-dir".into());
        };
        let source = if release == "image" {
            self.args.tigerbeetle.clone()
        } else {
            self.args.releases.join(release).join("tigerbeetle")
        };
        let bytes = install_binary(&source, &dir)?;
        let value = json!({"release": release, "bytes": bytes,
                           "at": self.booted.elapsed().as_secs_f64()});
        say(json!({"event": "installed", "install": value}));
        self.installs.push(value);
        Ok(json!({"ok": true, "installed": release, "bytes": bytes}))
    }

    /// Run the delayed installs that are due.
    fn install_due(&mut self) {
        let now = Instant::now();
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.pending = later;
        for (_, release) in due {
            if let Err(error) = self.install(&release) {
                say(json!({"event": "install-failed", "release": release, "error": error}));
            }
        }
    }

    fn format(&mut self) -> Result<(), String> {
        let mut command = self.tigerbeetle();
        command
            .arg("format")
            .arg(format!("--cluster={}", self.args.cluster))
            .arg(format!("--replica={}", self.args.replica))
            .arg(format!("--replica-count={}", self.args.peers.len()))
            .args(&self.args.format_args)
            .arg(&self.args.data);
        let started = Instant::now();
        let status = command.status().map_err(|e| format!("format: {e}"))?;
        say(
            json!({"event": "formatted", "status": status.code(), "seconds": started.elapsed().as_secs_f64()}),
        );
        if status.success() {
            Ok(())
        } else {
            Err(format!("format exited {status}"))
        }
    }

    fn start(&mut self) -> Result<Value, String> {
        if self.replica.is_some() {
            return Ok(json!({"ok": true, "already": true}));
        }
        if self.recovering.is_some() {
            return Err("recovering".into());
        }
        let mut command = self.tigerbeetle();
        command
            .arg("start")
            .arg(format!("--addresses={}", self.addresses))
            .args(&self.args.start_args)
            .arg(&self.args.data);
        let child = self.spawn(command).map_err(|e| format!("start: {e}"))?;
        self.starts += 1;
        self.killed = false;
        self.respawn_at = None;
        say(json!({"event": "started", "pid": child.id(), "starts": self.starts}));
        let pid = child.id();
        self.replica = Some(child);
        Ok(json!({"ok": true, "pid": pid}))
    }

    fn remove_data(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.args.data) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", self.args.data.display())),
        }
    }

    fn command(&mut self, line: &str) -> Value {
        let mut words = line.split_whitespace();
        let verb = words.next().unwrap_or("");
        let argument = words.next();
        let result = match (verb, argument) {
            ("status", _) => Ok(self.status()),
            ("start", _) => self.start(),
            ("install", Some(release)) => match words.next().map(str::parse::<f64>) {
                None => self.install(release),
                Some(Ok(delay)) if delay >= 0.0 => {
                    let due = Instant::now() + Duration::from_secs_f64(delay);
                    self.pending.push((due, release.to_string()));
                    Ok(json!({"ok": true, "pending": release, "in": delay}))
                }
                Some(_) => Err(format!("install {release}: bad delay")),
            },
            ("reformat", _) => {
                if self.replica.is_some() {
                    Err("replica running".into())
                } else {
                    say(json!({"event": "reformat"}));
                    self.remove_data()
                        .and_then(|()| self.format())
                        .and_then(|()| self.start())
                }
            }
            ("recover", _) => {
                if self.replica.is_some() || self.recovering.is_some() {
                    Err("replica running".into())
                } else {
                    say(json!({"event": "recover"}));
                    self.remove_data().and_then(|()| {
                        let mut command = self.tigerbeetle();
                        command
                            .arg("recover")
                            .arg(format!("--cluster={}", self.args.cluster))
                            .arg(format!("--addresses={}", self.addresses))
                            .arg(format!("--replica={}", self.args.replica))
                            .arg(format!("--replica-count={}", self.args.peers.len()))
                            .args(&self.args.format_args)
                            .arg(&self.args.data);
                        let child = self.spawn(command).map_err(|e| format!("recover: {e}"))?;
                        let pid = child.id();
                        self.recovering = Some(child);
                        Ok(json!({"ok": true, "recovering": pid}))
                    })
                }
            }
            ("kill", _) => match self.replica.as_mut() {
                Some(child) => {
                    let _ = child.kill();
                    self.killed = true;
                    Ok(json!({"ok": true}))
                }
                None => Err("not running".into()),
            },
            _ => Err(format!("unknown command {:?}", line.trim())),
        };
        match result {
            Ok(value) => value,
            Err(error) => json!({"ok": false, "error": error}),
        }
    }

    fn status(&self) -> Value {
        let size = std::fs::metadata(&self.args.data).ok().map(|m| m.len());
        let output = self.output.lock().unwrap();
        json!({"ok": true, "replica": self.args.replica, "running": self.replica.is_some(),
               "recovering": self.recovering.is_some(), "starts": self.starts,
               "respawns": self.respawns, "release": output.release,
               "panic_lines": output.panics, "installs": self.installs,
               "view": output.view, "role": output.role,
               "exits": self.exits, "notable": output.notable, "data_bytes": size})
    }

    fn exited(&mut self, status: &std::process::ExitStatus, what: &str) -> Value {
        // Let the output threads drain the last lines.
        std::thread::sleep(Duration::from_millis(100));
        let mut exit = exit_json(status);
        let output = self.output.lock().unwrap();
        exit["what"] = json!(what);
        exit["at"] = json!(self.booted.elapsed().as_secs_f64());
        exit["release"] = json!(output.release);
        exit["killed"] = json!(self.killed);
        exit["tail"] = json!(output.tail);
        exit
    }

    fn reap(&mut self) {
        if let Some(mut child) = self.replica.take() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let exit = self.exited(&status, "replica");
                    say(
                        json!({"event": "replica-exited", "exit": exit, "mem": memory(None, &self.args.data)}),
                    );
                    self.exits.push(exit);
                    if !self.killed && self.respawns < self.args.respawn {
                        self.respawn_at = Some(Instant::now() + Duration::from_secs(1));
                    }
                }
                _ => self.replica = Some(child),
            }
        }
        if self.replica.is_none() && self.respawn_at.is_some_and(|at| Instant::now() >= at) {
            self.respawns += 1;
            say(json!({"event": "respawn", "respawns": self.respawns}));
            if let Err(error) = self.start() {
                say(json!({"event": "start-failed", "error": error}));
            }
        }
        if let Some(mut child) = self.recovering.take() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let exit = self.exited(&status, "recover");
                    say(json!({"event": "recover-exited", "exit": exit}));
                    if status.success() {
                        if let Err(error) = self.start() {
                            say(json!({"event": "start-failed", "error": error}));
                        }
                    } else {
                        self.exits.push(exit);
                    }
                }
                _ => self.recovering = Some(child),
            }
        }
    }
}

/// Copy `source` into `dir/tigerbeetle` through a temporary file: write,
/// fsync, rename, fsync the directory. Returns the size.
fn install_binary(source: &Path, dir: &Path) -> Result<u64, String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let target = dir.join("tigerbeetle");
    let temporary = dir.join("tigerbeetle.installing");
    let bytes = std::fs::copy(source, &temporary)
        .map_err(|e| format!("copy {} to {}: {e}", source.display(), temporary.display()))?;
    let file = std::fs::File::open(&temporary).map_err(|e| e.to_string())?;
    file.set_permissions(std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(&temporary, &target).map_err(|e| format!("rename: {e}"))?;
    if let Ok(directory) = std::fs::File::open(dir) {
        let _ = directory.sync_all();
    }
    Ok(bytes)
}

/// Guest memory, the replica's resident size and the data file's allocated size.
fn memory(replica: Option<u32>, data: &Path) -> Value {
    let mut out = serde_json::Map::new();
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                let key = key.trim_end_matches(':');
                if matches!(
                    key,
                    "MemTotal" | "MemAvailable" | "MemFree" | "Shmem" | "Cached"
                ) {
                    out.insert(
                        format!("{key}_kb"),
                        value.parse::<u64>().unwrap_or(0).into(),
                    );
                }
            }
        }
    }
    if let Some(pid) = replica {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            if let Some(line) = text.lines().find(|l| l.starts_with("VmRSS:")) {
                let kb: u64 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                out.insert("replica_rss_kb".into(), kb.into());
            }
        }
    }
    if let Ok(meta) = std::fs::metadata(data) {
        use std::os::unix::fs::MetadataExt;
        out.insert("data_bytes".into(), meta.len().into());
        out.insert("data_allocated_bytes".into(), (meta.blocks() * 512).into());
    }
    Value::Object(out)
}

fn exit_json(status: &std::process::ExitStatus) -> Value {
    use std::os::unix::process::ExitStatusExt;
    json!({"code": status.code(), "signal": status.signal()})
}

/// `statfs` magic of ramfs.
const RAMFS_MAGIC: u64 = 0x8584_58f6;

fn fs_magic(dir: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statfs on a NUL-terminated path into a zeroed struct.
    unsafe {
        let mut stat: libc::statfs = std::mem::zeroed();
        (libc::statfs(path.as_ptr(), &mut stat) == 0).then_some(stat.f_type as u64)
    }
}

fn mount_ramfs(dir: &Path) -> Result<bool, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if fs_magic(dir) == Some(RAMFS_MAGIC) {
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    {
        let target = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: NUL-terminated strings; no mount data.
        let rc = unsafe {
            libc::mount(
                c"ramfs".as_ptr(),
                target.as_ptr(),
                c"ramfs".as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(format!(
                "mount ramfs {}: {}",
                dir.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(true)
    }
    #[cfg(not(target_os = "linux"))]
    Err("ramfs needs Linux".into())
}

pub fn run(args: Args) -> u8 {
    if let Some(dir) = &args.ramfs {
        match mount_ramfs(dir) {
            Ok(mounted) => {
                say(json!({"event": "ramfs", "dir": dir.display().to_string(), "mounted": mounted}))
            }
            Err(error) => {
                say(json!({"event": "fatal", "error": error}));
                return 2;
            }
        }
    }
    for dir in &args.probe {
        say(json!({"io_probe": probe(dir)}));
    }
    let mut addresses = Vec::new();
    for peer in &args.peers {
        match resolve(peer, args.port, Duration::from_secs(120)) {
            Ok(address) => addresses.push(address),
            Err(error) => {
                say(json!({"event": "fatal", "error": error}));
                return 2;
            }
        }
    }
    let mut agent = Agent {
        addresses: addresses.join(","),
        args,
        replica: None,
        recovering: None,
        exits: Vec::new(),
        starts: 0,
        output: Arc::new(Mutex::new(Output::default())),
        booted: Instant::now(),
        killed: false,
        respawns: 0,
        respawn_at: None,
        installs: Vec::new(),
        pending: Vec::new(),
    };
    if let Some(parent) = agent.args.data.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if agent.args.binary_dir.is_some() && !agent.binary().exists() {
        let release = agent.args.release.clone().unwrap_or_else(|| "image".into());
        if let Err(error) = agent.install(&release) {
            say(json!({"event": "fatal", "error": error}));
            return 2;
        }
    }
    let fresh = !agent.args.data.exists();
    say(
        json!({"event": "boot", "replica": agent.args.replica, "addresses": agent.addresses, "fresh": fresh,
               "binary": agent.binary().display().to_string()}),
    );
    if fresh {
        if let Err(error) = agent.format() {
            say(json!({"event": "fatal", "error": error}));
            return 2;
        }
    } else if agent.args.start_on_reboot {
        if let Err(error) = agent.start() {
            say(json!({"event": "start-on-reboot", "error": error}));
        }
    }
    let listener = match TcpListener::bind(("0.0.0.0", agent.args.agent_port)) {
        Ok(listener) => listener,
        Err(error) => {
            say(
                json!({"event": "fatal", "error": format!("bind {}: {error}", agent.args.agent_port)}),
            );
            return 2;
        }
    };
    let _ = listener.set_nonblocking(true);
    let mut reported = Instant::now() - Duration::from_secs(60);
    loop {
        if reported.elapsed() >= Duration::from_secs(5) {
            reported = Instant::now();
            let pid = agent.replica.as_ref().map(Child::id);
            say(json!({"mem": memory(pid, &agent.args.data)}));
        }
        agent.reap();
        agent.install_due();
        match listener.accept() {
            Ok((stream, _)) => serve(&mut agent, stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn serve(agent: &mut Agent, stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let reply = agent.command(&line);
    // Status is polled every second; log only the commands that act.
    if line.trim() != "status" {
        say(json!({"event": "command", "command": line.trim(), "reply": reply}));
    }
    let mut stream = stream;
    let _ = stream.write_all(format!("{reply}\n").as_bytes());
}

/// Send one command to a node agent and read its JSON reply.
pub fn command(host: &str, port: u16, line: &str, timeout: Duration) -> Result<Value, String> {
    let address = resolve(host, port, timeout)?;
    let stream = TcpStream::connect_timeout(
        &address.parse().map_err(|e| format!("{address}: {e}"))?,
        timeout,
    )
    .map_err(|e| format!("{host}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writer
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("{host}: {e}"))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| format!("{host}: {e}"))?;
    serde_json::from_str(&reply).map_err(|e| format!("{host}: reply {reply:?}: {e}"))
}

// ------------------------------------------------------------ the I/O probe

/// O_DIRECT and io_uring on `dir`: what TigerBeetle's storage needs.
pub fn probe(dir: &Path) -> Value {
    #[cfg(target_os = "linux")]
    {
        linux_probe(dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        json!({"dir": dir.display().to_string(), "error": "not linux"})
    }
}

#[cfg(target_os = "linux")]
fn linux_probe(dir: &Path) -> Value {
    use io_uring::{opcode, types, IoUring};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let mut report = serde_json::Map::new();
    report.insert("dir".into(), dir.display().to_string().into());
    let _ = std::fs::create_dir_all(dir);
    let fs_type = fs_magic(dir).map(|m| format!("{m:#x}"));
    report.insert("statfs_type".into(), json!(fs_type));
    let path = dir.join("seedbox-io-probe");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_DIRECT | libc::O_DSYNC)
        .open(&path);
    let file = match file {
        Ok(file) => {
            report.insert("o_direct_open".into(), true.into());
            file
        }
        Err(error) => {
            report.insert("o_direct_open".into(), false.into());
            report.insert("error".into(), error.to_string().into());
            return Value::Object(report);
        }
    };
    let fd = file.as_raw_fd();
    // SAFETY: fallocate on an open regular file.
    let fallocate = unsafe { libc::fallocate(fd, 0, 0, 64 << 20) };
    report.insert(
        "fallocate_64mib".into(),
        if fallocate == 0 {
            json!(true)
        } else {
            json!(std::io::Error::last_os_error().to_string())
        },
    );
    const SIZE: usize = 1 << 20;
    let layout = std::alloc::Layout::from_size_align(SIZE, 4096).unwrap();
    // SAFETY: two zeroed, 4096-aligned 1 MiB buffers, freed below.
    let (write, read) = unsafe {
        (
            std::alloc::alloc_zeroed(layout),
            std::alloc::alloc_zeroed(layout),
        )
    };
    for i in 0..SIZE {
        // SAFETY: within the allocation.
        unsafe { *write.add(i) = (i * 7 + 13) as u8 };
    }
    let result = (|| -> Result<bool, String> {
        let mut ring = IoUring::new(8).map_err(|e| format!("io_uring_setup: {e}"))?;
        let entries = [
            opcode::Write::new(types::Fd(fd), write, SIZE as u32)
                .offset(4096)
                .build()
                .user_data(1),
            opcode::Fsync::new(types::Fd(fd)).build().user_data(2),
        ];
        for (index, entry) in entries.iter().enumerate() {
            // SAFETY: the buffers outlive the submission, which is waited for.
            unsafe { ring.submission().push(entry).map_err(|e| e.to_string())? };
            ring.submit_and_wait(1)
                .map_err(|e| format!("submit: {e}"))?;
            let cqe = ring.completion().next().ok_or("no completion")?;
            if cqe.result() < 0 {
                return Err(format!(
                    "op {index}: {}",
                    std::io::Error::from_raw_os_error(-cqe.result())
                ));
            }
            if index == 0 && cqe.result() as usize != SIZE {
                return Err(format!("short write {}", cqe.result()));
            }
        }
        let entry = opcode::Read::new(types::Fd(fd), read, SIZE as u32)
            .offset(4096)
            .build()
            .user_data(3);
        // SAFETY: as above.
        unsafe { ring.submission().push(&entry).map_err(|e| e.to_string())? };
        ring.submit_and_wait(1)
            .map_err(|e| format!("submit: {e}"))?;
        let cqe = ring.completion().next().ok_or("no completion")?;
        if cqe.result() < 0 {
            return Err(format!(
                "read: {}",
                std::io::Error::from_raw_os_error(-cqe.result())
            ));
        }
        // SAFETY: both buffers are SIZE bytes.
        Ok(unsafe {
            std::slice::from_raw_parts(write, SIZE) == std::slice::from_raw_parts(read, SIZE)
        })
    })();
    // SAFETY: allocated above with this layout.
    unsafe {
        std::alloc::dealloc(write, layout);
        std::alloc::dealloc(read, layout);
    }
    match result {
        Ok(same) => {
            report.insert("io_uring_direct_write_read".into(), true.into());
            report.insert("read_back_matches".into(), same.into());
        }
        Err(error) => {
            report.insert("io_uring_direct_write_read".into(), false.into());
            report.insert("error".into(), error.into());
        }
    }
    drop(file);
    let _ = std::fs::remove_file(&path);
    Value::Object(report)
}

/// The view and role of a `transition_to_normal_from_*` line, such as
/// `0: transition_to_normal_from_view_change_status: view=1..2 primary`.
fn normal_view(line: &str) -> Option<(u64, String)> {
    let rest = &line[line.find("transition_to_normal_from")?..];
    let view = rest[rest.find("view=")? + 5..].split_whitespace().next()?;
    let view = view.rsplit("..").next()?.parse().ok()?;
    let role = rest.split_whitespace().last()?;
    matches!(role, "primary" | "backup").then(|| (view, role.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_come_from_transitions_to_normal() {
        let line = "2026-10-15 01:30:54.669Z info(replica): 1: transition_to_normal_from_view_change_status: view=1..4 primary";
        assert_eq!(normal_view(line), Some((4, "primary".into())));
        let line = "info(replica): 2: transition_to_normal_from_recovering_status: view=0 backup";
        assert_eq!(normal_view(line), Some((0, "backup".into())));
        assert_eq!(
            normal_view("info(replica): 0: transition_to_view_change_status: view=0..1 status=x"),
            None
        );
        assert!(notable(
            "info(replica): 2: release_transition: release=0.16.25..0.16.26 (reason=open)"
        ));
        assert!(!notable(
            "warning(message_bus): error connecting to replica 1: error.ConnectionRefused"
        ));
    }
}
