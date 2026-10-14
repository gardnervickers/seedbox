//! The committed scenario projects (`harness/jepsen/<system>/*.yaml`) and the
//! helpers that write them: machine sets with data directories on volumes
//! (a guest's root filesystem is RAM), images pinned by index digest, memory
//! sizing, and the oracle wiring that runs this crate's static binary.
//! Run `seedbox-jepsen gen` after changing a scenario; the files are committed.
use serde_json::Value;
use std::fmt::Write as _;

/// Where the oracle binary lands in a guest.
pub const BINARY: &str = "/jepsen/seedbox-jepsen";
/// The repository root, relative to a scenario file in `harness/jepsen/<system>/`.
pub const ROOT: &str = "../../..";
/// The image with the binary `seedbox-jepsen build-oracle` writes.
pub const DOCKERFILE: &str = "crates/seedbox-jepsen/guest.Dockerfile";

/// One machine of a project.
#[derive(Clone, Debug, Default)]
pub struct Machine {
    pub comments: Vec<String>,
    pub image: Option<String>,
    pub build: Option<(String, Option<String>)>,
    pub entrypoint: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    pub environment: Vec<(String, String)>,
    pub memory_mib: Option<u32>,
    pub cpu_speed: Option<String>,
    pub files: Vec<(String, String)>,
    pub volumes: Vec<(String, String)>,
}

impl Machine {
    pub fn image(image: &str) -> Self {
        Self {
            image: Some(image.into()),
            ..Self::default()
        }
    }

    pub fn build(context: &str, dockerfile: Option<&str>) -> Self {
        Self {
            build: Some((context.into(), dockerfile.map(str::to_owned))),
            ..Self::default()
        }
    }

    /// This crate's binary running `args` (a subcommand and its flags).
    pub fn program(args: &[String]) -> Self {
        let mut entrypoint = vec![BINARY.to_string()];
        entrypoint.extend(args.iter().cloned());
        Self {
            build: Some((ROOT.into(), Some(DOCKERFILE.into()))),
            entrypoint: Some(entrypoint),
            memory_mib: Some(256),
            ..Self::default()
        }
    }

    pub fn entrypoint(mut self, args: &[&str]) -> Self {
        self.entrypoint = Some(args.iter().map(|s| s.to_string()).collect());
        self
    }

    pub fn command(mut self, args: Vec<String>) -> Self {
        self.command = Some(args);
        self
    }

    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.environment.push((key.into(), value.into()));
        self
    }

    pub fn memory(mut self, mib: u32) -> Self {
        self.memory_mib = Some(mib);
        self
    }

    pub fn cpu(mut self, speed: &str) -> Self {
        self.cpu_speed = Some(speed.into());
        self
    }

    pub fn file(mut self, source: &str, target: &str) -> Self {
        self.files.push((source.into(), target.into()));
        self
    }

    /// Keep `target` on a fresh disk so it outlives a crash and is not RAM.
    pub fn data(mut self, target: &str) -> Self {
        self.volumes.push(("data".into(), target.into()));
        self
    }

    pub fn comment(mut self, text: &str) -> Self {
        self.comments.push(text.into());
        self
    }
}

/// `n1..nN` (or another prefix).
pub fn nodes(prefix: &str, count: usize) -> Vec<String> {
    (1..=count).map(|i| format!("{prefix}{i}")).collect()
}

/// A whole project.
pub struct Scenario {
    pub name: String,
    pub comment: String,
    pub generated: String,
    pub machines: Vec<(String, Machine)>,
    pub oracle: Machine,
    pub latency: String,
}

fn quote(text: &str) -> String {
    Value::from(text).to_string()
}

fn flow(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| quote(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn machine(out: &mut String, indent: &str, m: &Machine) {
    for comment in &m.comments {
        let _ = writeln!(out, "{indent}# {comment}");
    }
    if let Some(image) = &m.image {
        let _ = writeln!(out, "{indent}image: {image}");
    }
    if let Some((context, dockerfile)) = &m.build {
        let _ = writeln!(out, "{indent}build:\n{indent}  context: {context}");
        if let Some(dockerfile) = dockerfile {
            let _ = writeln!(out, "{indent}  dockerfile: {dockerfile}");
        }
    }
    if let Some(entrypoint) = &m.entrypoint {
        let _ = writeln!(out, "{indent}entrypoint: {}", flow(entrypoint));
    }
    if let Some(command) = &m.command {
        let _ = writeln!(out, "{indent}command: {}", flow(command));
    }
    if !m.environment.is_empty() {
        let _ = writeln!(out, "{indent}environment:");
        for (key, value) in &m.environment {
            let _ = writeln!(out, "{indent}  {key}: {}", quote(value));
        }
    }
    if let Some(mib) = m.memory_mib {
        let _ = writeln!(out, "{indent}memoryMiB: {mib}");
    }
    if let Some(speed) = &m.cpu_speed {
        let _ = writeln!(out, "{indent}cpu:\n{indent}  speed: {speed}");
    }
    if !m.files.is_empty() {
        let _ = writeln!(out, "{indent}files:");
        for (source, target) in &m.files {
            let _ = writeln!(
                out,
                "{indent}- source: {source}\n{indent}  target: {target}"
            );
        }
    }
    if !m.volumes.is_empty() {
        let _ = writeln!(out, "{indent}volumes:");
        for (name, target) in &m.volumes {
            let _ = writeln!(out, "{indent}  {name}:\n{indent}    target: {target}");
        }
    }
}

impl Scenario {
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in self.comment.lines() {
            let _ = writeln!(out, "# {line}");
        }
        let _ = writeln!(
            out,
            "# Generated by `seedbox-jepsen gen`: {}.",
            self.generated
        );
        let _ = writeln!(out, "name: {}", self.name);
        if !self.machines.is_empty() {
            let _ = writeln!(out, "machines:");
            for (name, m) in &self.machines {
                let _ = writeln!(out, "  {name}:");
                machine(&mut out, "    ", m);
            }
        }
        let _ = writeln!(out, "oracle:");
        machine(&mut out, "  ", &self.oracle);
        let _ = writeln!(out, "network:\n  latency: {}", self.latency);
        out
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ------------------------------------------------------------ NATS

const NATS: [(&str, &str); 3] = [
    ("2.12.1", "docker.io/library/nats:2.12.1@sha256:2b8b92b0bb66fa75439511310efaa28e0da2a0c2e47b626027530aa8e60fbc3b"),
    ("2.10.22", "docker.io/library/nats:2.10.22@sha256:d15749dc10d8e67b55f496551ca3794b2f131556342b313eb3e6115ec75f6fb3"),
    ("2.10.23", "docker.io/library/nats:2.10.23@sha256:9df6b07d695b48b409669347f43c78b1a8fe7b5ae7ec465a661922de9b2048f1"),
];

fn lookup(table: &[(&str, &'static str)], key: &str) -> &'static str {
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .expect("pinned version")
}

#[allow(clippy::type_complexity)]
fn nats() -> Vec<Scenario> {
    // (name, comment, version, config, oracle arguments, server memory)
    let specs: [(&str, &str, &str, &str, &[&str], u32); 10] = [
        ("nats-healthy", "Baseline: the NATS JetStream queue workload with no faults keeps every\nacknowledged message on every server.",
         "2.12.1", "default", &["--mode", "none", "--warmup", "20", "--hold", "0", "--after", "0"], 512),
        ("nats-7564-coordinated-crash", "nats-server#7564: the default lazy fsync acknowledges writes it has not synced;\ncrash every machine together and acknowledged messages are lost.",
         "2.12.1", "default", &["--mode", "coordinated", "--warmup", "30", "--after", "15"], 512),
        ("nats-7564-control-always", "Negative control for nats-server#7564: `sync_interval: always` under the same\ncoordinated crash must keep every acknowledged message.",
         "2.12.1", "always", &["--mode", "coordinated", "--warmup", "30", "--after", "15"], 512),
        ("nats-7567-pause-crash", "nats-server#7567 with Jepsen's pause-kill nemesis and sync_interval 10s: pause n1 n2,\ncrash n3, pause n4 n5 and resume n1 n2, reboot n3, then resume n4 n5. One power\nfailure loses acknowledged messages and leaves the replicas divergent.",
         "2.12.1", "sync10s", &["--mode", "pause-crash", "--warmup", "15", "--hold", "30", "--after", "10"], 2048),
        ("nats-7567-part-crash", "nats-server#7567 with Jepsen's part-kill nemesis and sync_interval 10s: partition\nn1 n2 away, crash n3, move the partition to n4 n5, reboot n3, then heal.",
         "2.12.1", "sync10s", &["--mode", "part-crash", "--warmup", "15", "--hold", "30", "--after", "10"], 2048),
        ("nats-7567-control-always", "Negative control for nats-server#7567: the pause-kill schedule with\n`sync_interval: always` loses nothing in the power failure.",
         "2.12.1", "always", &["--mode", "pause-crash", "--warmup", "15", "--hold", "30", "--after", "10"], 2048),
        ("nats-6888-kill", "nats-server#6888 (2.10.20-2.10.22): repeated SIGKILL and restart of random servers soon\nafter the stream is created can make the meta layer forget the stream.",
         "2.10.22", "default", &["--mode", "kill", "--warmup", "5", "--hold", "60", "--after", "10"], 512),
        ("nats-6888-kill-all", "nats-server#6888 as nats-server#5946 reproduced it: SIGKILL all five servers two\nseconds after the stream is created, then restart them.",
         "2.10.22", "default", &["--mode", "kill-all", "--warmup", "2", "--hold", "30", "--after", "10"], 512),
        ("nats-6888-kill-all-fixed", "Negative control for nats-6888-kill-all on 2.10.23.",
         "2.10.23", "default", &["--mode", "kill-all", "--warmup", "2", "--hold", "30", "--after", "10"], 512),
        ("nats-6888-control-fixed", "Negative control for nats-server#6888: the same kill schedule on 2.10.23, which\ncarries the fix.",
         "2.10.23", "default", &["--mode", "kill", "--warmup", "5", "--hold", "60", "--after", "10"], 512),
    ];
    specs
        .iter()
        .map(|(name, comment, version, config, arguments, memory)| {
            let machines = nodes("n", 5)
                .into_iter()
                .map(|node| {
                    let server = Machine::image(lookup(&NATS, version))
                        .entrypoint(&["/nats-server", "-c", "/etc/nats/nats.conf"])
                        .env("SERVER_NAME", node.clone())
                        .memory(*memory)
                        .file(&format!("nats-{config}.conf"), "/etc/nats/nats.conf")
                        .data("/data");
                    (node, server)
                })
                .collect();
            let mut args = strings(&["nats"]);
            args.extend(strings(arguments));
            args.extend(["--config".into(), format!("{version}/{config}")]);
            Scenario {
                name: name.to_string(),
                comment: comment.to_string(),
                generated: format!("NATS {version}, nats-{config}.conf"),
                machines,
                oracle: Machine::program(&args),
                latency: "1ms".into(),
            }
        })
        .collect()
}

// ------------------------------------------------------------ etcd

const ETCD_352: &str = "quay.io/coreos/etcd:v3.5.2@sha256:70fe70b6cdaea99fa9e99d3a16483954eb084c3b32a2475abea4e709a793636c";

fn etcd() -> Vec<Scenario> {
    // (name, comment, mode, guard, 3.5.2)
    let specs: [(&str, &str, &str, &str, bool); 10] = [
        ("etcd-healthy", "Baseline: the lock-set workload with no faults.", "none", "none", false),
        ("etcd-lock-pause-holder", "etcd locks do not give mutual exclusion: pause the lock holder's client process\n(a GC pause) for 3 s, longer than the 2 s lease TTL, after it has read the set.\nIts lease expires, another client takes the lock, and the paused client resumes\nand writes its stale set: acknowledged updates are lost.", "pause-holder", "none", false),
        ("etcd-11456-pause-waiter-member", "etcd#11456 through a process pause: when the holder has read the set, pause the\nmember of a client waiting for the lock for 3 s. The waiter's lease expires while\nits Lock RPC is pending there, and the RPC then succeeds without its key.", "pause-waiter-member", "none", false),
        ("etcd-lock-pause-random-member", "Jepsen's lock-set nemesis: pause a random etcd member for 3 s every 5 s.", "pause-random-member", "none", false),
        ("etcd-11456-partition-waiter", "etcd#11456: while one client holds the lock, isolate the member of a waiting client\nfrom the other members for 3 s. The waiter's lease expires; after the heal its\nLock RPC still returns success without checking that its key exists.", "partition-waiter", "none", false),
        ("etcd-control-token-pause-holder", "Negative control: the pause-holder schedule, with every read and write fenced by\nthe lock key's create revision. The lock still fails, but no update is lost.", "pause-holder", "token", false),
        ("etcd-control-txn-pause-holder", "Negative control: the pause-holder schedule with the set stored in etcd and every\nwrite a transaction that requires the client's lock key to exist.", "pause-holder", "txn", false),
        ("etcd-control-token-partition-waiter", "Negative control for etcd#11456: the partition-waiter schedule with fencing tokens.", "partition-waiter", "token", false),
        ("etcd-11456-control-v352-partition-waiter", "Negative control for etcd#11456: the partition-waiter schedule on etcd 3.5.2, whose\nLock RPC fails with a session-expired error instead of granting a deleted key.", "partition-waiter", "none", true),
        ("etcd-11456-control-v352-pause-waiter-member", "Negative control for etcd#11456: the pause-waiter-member schedule on etcd 3.5.2.", "pause-waiter-member", "none", true),
    ];
    let members = nodes("n", 5);
    let cluster: Vec<String> = members
        .iter()
        .map(|m| format!("{m}=http://{m}:2380"))
        .collect();
    let cluster = cluster.join(",");
    specs
        .iter()
        .map(|(name, comment, mode, guard, v352)| {
            let mut machines: Vec<(String, Machine)> = members
                .iter()
                .map(|member| {
                    let base = if *v352 {
                        // The 3.5.2 image's root archive needs a 648 MiB boot floor.
                        Machine::image(ETCD_352).memory(768)
                    } else {
                        Machine::build("image", None).memory(512)
                    };
                    let server = base
                        .entrypoint(&["/usr/local/bin/etcd"])
                        .command(vec![
                            format!("--name={member}"),
                            format!("--data-dir=/var/lib/etcd/{member}"),
                            "--listen-client-urls=http://0.0.0.0:2379".into(),
                            format!("--advertise-client-urls=http://{member}:2379"),
                            "--listen-peer-urls=http://0.0.0.0:2380".into(),
                            format!("--initial-advertise-peer-urls=http://{member}:2380"),
                            format!("--initial-cluster={cluster}"),
                            "--initial-cluster-state=new".into(),
                            "--logger=zap".into(),
                            "--log-level=info".into(),
                        ])
                        .env("ETCD_UNSUPPORTED_ARCH", "arm64")
                        .data("/var/lib/etcd");
                    (member.clone(), server)
                })
                .collect();
            for (index, member) in members.iter().enumerate() {
                let client = format!("c{}", index + 1);
                let args = vec![
                    "etcd-client".into(),
                    "--name".into(),
                    client.clone(),
                    "--member".into(),
                    member.clone(),
                    "--guard".into(),
                    guard.to_string(),
                ];
                machines.push((client, Machine::program(&args)));
            }
            let args = strings(&[
                "etcd",
                "--mode",
                mode,
                "--guard",
                guard,
                "--duration",
                "120",
            ]);
            Scenario {
                name: name.to_string(),
                comment: comment.to_string(),
                generated: format!(
                    "etcd {}, mode {mode}, guard {guard}",
                    if *v352 { "3.5.2" } else { "3.4.3" }
                ),
                machines,
                oracle: Machine::program(&args),
                latency: "1ms".into(),
            }
        })
        .collect()
}

// ------------------------------------------------------------ PostgreSQL

const POSTGRES: [(&str, &str); 2] = [
    ("12.3", "docker.io/library/postgres:12.3@sha256:a06e6e6e519b7a329c419f8221edec66cfc45511e8b80e262c12103ba745cf19"),
    ("12.4", "docker.io/library/postgres:12.4@sha256:a1e04460fdd3c338d6b65a2ab66b5aa2748eb18da3e55bcdc9ef17831ed3ad46"),
];

fn pg_server(version: &str, pgmon: &str, extra: &[&str]) -> Machine {
    let mut command = strings(&["postgres", "-c", "log_min_messages=log"]);
    command.extend(strings(extra));
    Machine::image(lookup(&POSTGRES, version))
        .memory(1024)
        .comment("pgmon.sh runs the official entrypoint and reports guest memory and OOM kills.")
        .entrypoint(&["bash", "/pgmon.sh"])
        .comment("Log no ERROR (every serialization failure would be four lines); keep LOG, FATAL, PANIC.")
        .command(command)
        .file(pgmon, "/pgmon.sh")
        .env("POSTGRES_HOST_AUTH_METHOD", "trust")
        // initdb needs an empty directory; the volume's root is not one.
        .env("PGDATA", "/var/lib/postgresql/data/pgdata")
        .data("/var/lib/postgresql/data")
}

fn postgres() -> Vec<Scenario> {
    let specs: [(&str, &str, &str, &str, u32); 5] = [
        ("pg123-serializable", "PostgreSQL 12.3 SERIALIZABLE allows G2-item (Jepsen, 2020-06-12): SSI misses an\nrw-conflict on a freshly inserted row that a third transaction is updating, so\nanti-dependency cycles commit. Fixed by 5940ffb in 12.4.", "12.3", "serializable", 60),
        ("pg124-serializable", "Negative control for pg123-serializable: 12.4 carries 5940ffb, so the same\nworkload must commit no dependency cycle.", "12.4", "serializable", 60),
        ("pg123-serializable-15s", "pg123-serializable with a 15 s workload, to measure how often one seed reproduces\nthe bug in a short window.", "12.3", "serializable", 15),
        ("pg124-serializable-15s", "Negative control for pg123-serializable-15s on 12.4.", "12.4", "serializable", 15),
        ("pg123-repeatable-read", "PostgreSQL REPEATABLE READ is snapshot isolation: G2-item (write skew) is expected\nby design; the history is checked against snapshot isolation, which allows it.", "12.3", "repeatable-read", 60),
    ];
    specs
        .iter()
        .map(|(name, comment, version, isolation, duration)| {
            let args: Vec<String> = vec![
                "postgres".into(),
                "--isolation".into(),
                isolation.to_string(),
                "--clients".into(),
                "20".into(),
                "--duration".into(),
                duration.to_string(),
                "--label".into(),
                format!("{version}/{isolation}"),
            ];
            Scenario {
                name: name.to_string(),
                comment: comment.to_string(),
                generated: format!("PostgreSQL {version}, {isolation}"),
                machines: vec![("pg".into(), pg_server(version, "pgmon.sh", &[]))],
                oracle: Machine::program(&args),
                latency: "1ms".into(),
            }
        })
        .collect()
}

fn postgres_stall() -> Scenario {
    let probe = Machine::program(&strings(&[
        "pg-probe", "--at", "13", "--at", "16", "--at", "19",
    ]));
    let args = strings(&[
        "postgres",
        "--isolation",
        "serializable",
        "--clients",
        "20",
        "--duration",
        "15",
        "--label",
        "12.3/serializable",
    ]);
    let mut scenario = Scenario {
        name: "pg123-stall-lockwaits".into(),
        comment: "Diagnostic for a Seedbox issue: pg123-serializable-15s plus log_lock_waits=on and an idle\n\
                  probe machine that reports pg_stat_activity and pg_locks at 13, 16 and 19 s. In a\n\
                  stalled seed two backends deadlock at startup and no deadlock_timeout (SIGALRM) fires\n\
                  afterwards, so the deadlock is never broken."
            .into(),
        generated: "PostgreSQL 12.3, serializable, log_lock_waits".into(),
        machines: vec![
            ("pg".into(), pg_server("12.3", "../pgmon.sh", &["-c", "log_lock_waits=on"])),
            ("probe".into(), probe),
        ],
        oracle: Machine::program(&args),
        latency: "1ms".into(),
    };
    // One directory deeper than the other PostgreSQL scenarios.
    for m in scenario
        .machines
        .iter_mut()
        .map(|(_, m)| m)
        .chain([&mut scenario.oracle])
    {
        if let Some((context, _)) = &mut m.build {
            if context == ROOT {
                context.insert_str(0, "../");
            }
        }
    }
    scenario
}

// ------------------------------------------------------------ Kafka

const KAFKA_JVM: &str = "-server -XX:+UseSerialGC -XX:TieredStopAtLevel=1 -XX:CICompilerCount=1 -XX:-UsePerfData -Djava.awt.headless=true";

struct KafkaSpec {
    name: &'static str,
    comment: String,
    version: &'static str,
    nemesis: &'static str,
    brokers: usize,
    duration: u32,
    workload: &'static str,
    extra: &'static str,
    txn_timeout: u32,
    /// The client's Kafka version, when it differs from the brokers'.
    client: Option<&'static str>,
}

fn kafka() -> Vec<Scenario> {
    let e2e = "KAFKA-17754 (a delayed EndTxn decides a later transaction): when the client prints\n\
               its marker just before commitTransaction/abortTransaction, cut c1 off from the brokers\n\
               for 3.5-5.5 s. The client times out the EndTxn, retries it on a new connection after\n\
               the heal and moves on; the first copy, still queued in c1's TCP stack, is retransmitted\n\
               later and commits or aborts whatever transaction the producer has open then.";
    let base = |name, comment: String, version, nemesis| KafkaSpec {
        name,
        comment,
        version,
        nemesis,
        brokers: 1,
        duration: 60,
        workload: "txn",
        extra: "",
        txn_timeout: 1000,
        client: None,
    };
    let specs = vec![
        KafkaSpec { duration: 30, ..base("kafka-healthy", "Baseline: Jepsen's transactional queue workload on Kafka 3.8.0, no faults.".into(), "3.8.0", "none") },
        KafkaSpec { txn_timeout: 60000, ..base("kafka-17754-end-partition", format!("{e2e}\nKafka 3.8.0 (the affected version), one broker, Kafka's default\ntransaction.timeout.ms (60 s)."), "3.8.0", "end-partition") },
        base("kafka-17754-end-partition-t1s", format!("{e2e}\nKafka 3.8.0, one broker, Jepsen's transaction.timeout.ms of 1 s: a cut that spans the\ncoordinator's 10 s timeout check aborts the transaction with an epoch bump, which fences\nthe delayed EndTxn."), "3.8.0", "end-partition"),
        KafkaSpec { txn_timeout: 60000, ..base("kafka-17754-control-v431-end-partition", "Negative control for KAFKA-17754: the end-partition schedule on Kafka 4.3.1 brokers and\nclient, with transaction.version 2 (KIP-890 part 2: every transaction bumps the producer\nepoch, so a delayed EndTxn carries a stale epoch and is fenced).".into(), "4.3.1", "end-partition") },
        KafkaSpec { txn_timeout: 60000, client: Some("3.8.0"), ..base("kafka-17754-v431-brokers-v380-client", "The end-partition schedule on Kafka 4.3.1 brokers (transaction.version 2) with the\nKafka 3.8.0 Java client, which still sends EndTxn version 4: no epoch bump per\ntransaction, so the fix needs a new client as well as new brokers.".into(), "4.3.1", "end-partition") },
        KafkaSpec { brokers: 3, txn_timeout: 60000, ..base("kafka-17754-end-partition-3", format!("{e2e}\nKafka 3.8.0, three brokers, replication factor 3."), "3.8.0", "end-partition") },
        KafkaSpec { brokers: 3, duration: 90, ..base("kafka-17754-jepsen", "Jepsen's untargeted nemesis on Kafka 3.8.0: every 10 s, pause a broker's process or cut\nc1 off from a broker for 2-10 s.".into(), "3.8.0", "jepsen") },
        KafkaSpec { brokers: 3, workload: "produce", ..base("kafka-crash-all", "acks=all without fsync: an idempotent acks=all producer writes to a topic with\nreplication factor 3 and min.insync.replicas 2; after 1000 acknowledgements every\nbroker machine loses power at once, then all reboot. Kafka's default flush settings\nleave acknowledged records in the page cache, so they are lost.".into(), "3.8.0", "crash-all") },
        KafkaSpec { brokers: 3, workload: "produce", extra: "log.flush.interval.messages=1", ..base("kafka-crash-all-control-flush", "Negative control: kafka-crash-all with log.flush.interval.messages=1, so every append\nis fsynced before it is acknowledged.".into(), "3.8.0", "crash-all") },
    ];
    specs
        .into_iter()
        .map(|spec| {
            let names = nodes("n", spec.brokers);
            let voters: Vec<String> = (1..=spec.brokers)
                .map(|i| format!("{i}@n{i}:9093"))
                .collect();
            let mut machines: Vec<(String, Machine)> = names
                .iter()
                .enumerate()
                .map(|(i, broker)| {
                    let mut m = Machine::build(
                        "image",
                        Some(&format!("kafka-{}.Dockerfile", spec.version)),
                    )
                    .env("KAFKA_NODE_ID", (i + 1).to_string())
                    .env("CLUSTER_ID", "4L6g3nShT-eMCtK--X86sw")
                    .env("KAFKA_VOTERS", voters.join(","))
                    .env("KAFKA_RF", spec.brokers.to_string())
                    .env("KAFKA_HEAP_OPTS", "-Xms256m -Xmx256m")
                    .env("KAFKA_JVM_PERFORMANCE_OPTS", KAFKA_JVM);
                    if !spec.extra.is_empty() {
                        m = m.env("SEEDBOX_EXTRA_PROPERTIES", spec.extra);
                    }
                    (broker.clone(), m.data("/data").memory(2048).cpu("4GHz"))
                })
                .collect();
            let bootstrap: Vec<String> = names.iter().map(|b| format!("{b}:9092")).collect();
            let command = vec![
                "--name".into(),
                "c1".into(),
                "--bootstrap".into(),
                bootstrap.join(","),
                "--rf".into(),
                spec.brokers.to_string(),
                "--mode".into(),
                spec.workload.into(),
                "--txn-timeout-ms".into(),
                spec.txn_timeout.to_string(),
            ];
            machines.push((
                "c1".into(),
                Machine::build(
                    "image",
                    Some(&format!(
                        "client-{}.Dockerfile",
                        spec.client.unwrap_or(spec.version)
                    )),
                )
                .command(command)
                .memory(1024)
                .cpu("4GHz"),
            ));
            let args = vec![
                "kafka".into(),
                "--workload".into(),
                spec.workload.into(),
                "--nemesis".into(),
                spec.nemesis.into(),
                "--brokers".into(),
                names.join(","),
                "--duration".into(),
                spec.duration.to_string(),
            ];
            Scenario {
                name: spec.name.into(),
                comment: spec.comment,
                generated: format!(
                    "Kafka {} brokers, {} client, {} broker(s), workload {}, nemesis {}",
                    spec.version,
                    spec.client.unwrap_or(spec.version),
                    spec.brokers,
                    spec.workload,
                    spec.nemesis
                ),
                machines,
                oracle: Machine::program(&args),
                latency: "1ms".into(),
            }
        })
        .collect()
}

// ------------------------------------------------------------ MySQL

/// One MySQL server: the official image without its /dev entries (see
/// `image/mysql-VERSION.Dockerfile`, which pins it), the official entrypoint under
/// mysqlmon.sh, its data directory on a volume, root with an empty password, and
/// `extra` options.
fn mysql_server(version: &str, extra: &[&str]) -> Machine {
    let mut command = strings(&[
        "mysqld",
        // initialize needs an empty directory; the volume's root is not one.
        "--datadir=/var/lib/mysql/data",
        "--innodb-buffer-pool-size=128M",
        "--max-connections=64",
        "--performance-schema=OFF",
        // Liveness only: a lock wait fails (a definite abort) before the client's 10 s timeout.
        "--innodb-lock-wait-timeout=5",
    ]);
    command.extend(strings(extra));
    Machine::build("image", Some(&format!("mysql-{version}.Dockerfile")))
        // The root filesystem (about 330 MB) lives in guest RAM next to mysqld.
        .memory(1536)
        .comment(
            "mysqlmon.sh runs the official entrypoint and reports guest memory and the data size.",
        )
        .entrypoint(&["bash", "/mysqlmon.sh"])
        .command(command)
        .file("mysqlmon.sh", "/mysqlmon.sh")
        .env("MYSQL_ALLOW_EMPTY_PASSWORD", "yes")
        .data("/var/lib/mysql")
}

#[allow(clippy::type_complexity)]
fn mysql() -> Vec<Scenario> {
    const SINGLE: &[&str] = &["--skip-log-bin"];
    // (name, comment, version, oracle arguments, server options)
    let specs: [(&str, &str, &str, &[&str], &[&str]); 11] = [
        ("mysql-rr-append", "MySQL 8.0.34 REPEATABLE READ is not Adya's repeatable read (Jepsen, 2023-12-19):\non one healthy node, list-append commits G-single, G2-item and lost updates (P4), and\nreads after a transaction's own write see other transactions' commits (internal).",
         "8.0.34", &["--workload", "append", "--isolation", "repeatable-read"], SINGLE),
        ("mysql-ser-append", "Negative control for mysql-rr-append: SERIALIZABLE must commit no anomaly.",
         "8.0.34", &["--workload", "append", "--isolation", "serializable"], SINGLE),
        ("mysql-rc-append", "Control for mysql-rr-append: READ COMMITTED (PL-2) must show no G0 or G1;\nG-single, G2-item, lost updates and non-repeatable reads are allowed.",
         "8.0.34", &["--workload", "append", "--isolation", "read-committed"], SINGLE),
        ("mysql846-rr-append", "mysql-rr-append on MySQL 8.4.6: the behaviour is by design, so RR anomalies remain.",
         "8.4.6", &["--workload", "append", "--isolation", "repeatable-read"], SINGLE),
        ("mysql-rr-nrr", "MySQL 8.0.34 REPEATABLE READ non-repeatable reads (Jepsen): a transaction reads a\nrow's name, updates its gender, and reads the name again, which then shows a concurrent\nrename, delete or re-insert, because InnoDB DML reads the latest committed row.",
         "8.0.34", &["--workload", "nrr", "--isolation", "repeatable-read"], SINGLE),
        ("mysql-ser-nrr", "Negative control for mysql-rr-nrr: SERIALIZABLE reads lock, so the name never changes.",
         "8.0.34", &["--workload", "nrr", "--isolation", "serializable"], SINGLE),
        ("mysql-rr-mav", "MySQL 8.0.34 REPEATABLE READ violates monotonic atomic view (Jepsen): a reader that\nupdates row 1 sees a writer's increment of row 1 but then reads row 0 without it.",
         "8.0.34", &["--workload", "mav", "--isolation", "repeatable-read"], SINGLE),
        ("mysql-ser-mav", "Negative control for mysql-rr-mav: SERIALIZABLE views stay monotonic.",
         "8.0.34", &["--workload", "mav", "--isolation", "serializable"], SINGLE),
        ("mysql-crash-flush0", "MySQL 8.0.34 with innodb_flush_log_at_trx_commit=0 acknowledges commits it has\nnot written: power-fail the node at 20 s and reboot it, and acknowledged appends\nare missing from the final read. SERIALIZABLE, so only durability can fail.",
         "8.0.34", &["--workload", "append", "--isolation", "serializable", "--duration", "45", "--crash-at", "20"],
         &["--skip-log-bin", "--innodb-flush-log-at-trx-commit=0"]),
        ("mysql-crash-flush1", "Negative control for mysql-crash-flush0: the default innodb_flush_log_at_trx_commit=1\nkeeps every acknowledged append through the same power failure.",
         "8.0.34", &["--workload", "append", "--isolation", "serializable", "--duration", "45", "--crash-at", "20"],
         &["--skip-log-bin", "--innodb-flush-log-at-trx-commit=1"]),
        ("mysql-crash-flush2", "mysql-crash-flush0 with innodb_flush_log_at_trx_commit=2: commits reach the OS but\nare synced once a second, so a power failure (not a process crash) loses them too.",
         "8.0.34", &["--workload", "append", "--isolation", "serializable", "--duration", "45", "--crash-at", "20"],
         &["--skip-log-bin", "--innodb-flush-log-at-trx-commit=2"]),
    ];
    let mut out: Vec<Scenario> = specs
        .iter()
        .map(|(name, comment, version, oracle, server)| {
            let mut args = strings(&["mysql"]);
            args.extend(strings(oracle));
            args.extend(strings(&["--label", &format!("{version}/{name}")]));
            Scenario {
                name: name.to_string(),
                comment: comment.to_string(),
                generated: format!("MySQL {version}, {}", oracle.join(" ")),
                machines: vec![("mysql".into(), mysql_server(version, server))],
                oracle: Machine::program(&args),
                latency: "1ms".into(),
            }
        })
        .collect();
    for (name, preserve, comment) in [
        ("mysql-replica-order", "OFF", "Jepsen's RDS finding on local binlog replication: with\nreplica_preserve_commit_order=OFF (RDS's default), multi-threaded replicas commit out\nof the primary's order, so SERIALIZABLE read-only transactions on replicas observe\nG-single and G2-item."),
        ("mysql-replica-order-on", "ON", "Negative control for mysql-replica-order: replica_preserve_commit_order=ON\n(the community default since 8.0.27) applies in the primary's commit order."),
    ] {
        let gtid = ["--gtid-mode=ON", "--enforce-gtid-consistency=ON"];
        let mut primary = vec!["--server-id=1", "--binlog-transaction-dependency-tracking=WRITESET"];
        primary.extend(gtid);
        let replica = |id: &'static str| -> Vec<String> {
            let mut options = strings(&[id, "--skip-log-bin", "--replica-parallel-workers=4",
                "--replica-parallel-type=LOGICAL_CLOCK"]);
            options.push(format!("--replica-preserve-commit-order={preserve}"));
            options.extend(strings(&gtid));
            options
        };
        let r1 = replica("--server-id=2");
        let r2 = replica("--server-id=3");
        let args = strings(&[
            "mysql", "--workload", "append", "--isolation", "serializable",
            "--replica", "r1", "--replica", "r2", "--label", &format!("8.0.34/{name}"),
        ]);
        out.push(Scenario {
            name: name.into(),
            comment: comment.into(),
            generated: format!("MySQL 8.0.34, primary and two replicas, replica_preserve_commit_order={preserve}"),
            machines: vec![
                ("mysql".into(), mysql_server("8.0.34", &primary)),
                ("r1".into(), mysql_server("8.0.34", &r1.iter().map(String::as_str).collect::<Vec<_>>())),
                ("r2".into(), mysql_server("8.0.34", &r2.iter().map(String::as_str).collect::<Vec<_>>())),
            ],
            oracle: Machine::program(&args),
            latency: "1ms".into(),
        });
    }
    out
}

// ------------------------------------------------------------ MariaDB Galera

/// One Galera node: the pinned MariaDB 12.1.2 image, slimmed
/// (`image/mariadb-12.1.2.Dockerfile`), under galera-node.sh with the shared
/// galera.cnf, its data directory on a volume, and `flush` as
/// innodb_flush_log_at_trx_commit.
fn galera_node(name: &str, flush: u8) -> Machine {
    Machine::build("image", Some("mariadb-12.1.2.Dockerfile"))
        // The root filesystem (about 200 MB) lives in guest RAM next to mariadbd.
        .memory(1280)
        .comment("galera-node.sh recovers the node's position and waits for the oracle to say bootstrap or join.")
        .entrypoint(&["bash", "/galera-node.sh"])
        .command(vec![
            format!("--wsrep-node-address={name}"),
            format!("--wsrep-node-name={name}"),
            format!("--innodb-flush-log-at-trx-commit={flush}"),
        ])
        .env("GALERA_NODE", name)
        .file("galera-node.sh", "/galera-node.sh")
        .file("galera.cnf", "/etc/mysql/mariadb.conf.d/99-jepsen-galera.cnf")
        .data("/var/lib/mysql")
}

#[allow(clippy::type_complexity)]
fn galera() -> Vec<Scenario> {
    // Recovery from a whole-cluster failure takes about 90 s of simulated time (reboot,
    // --wsrep-recover, bootstrap, two SSTs); run on afterwards so later appends land.
    const FAULT: &[&str] = &["--fault-at", "20", "--duration", "130"];
    // (name, comment, innodb_flush_log_at_trx_commit, oracle arguments)
    let specs: [(&str, &str, u8, Vec<&str>); 11] = [
        ("galera-healthy-append", "MDEV-38977 (Jepsen, MariaDB Galera Cluster 12.1.2): with no faults, list-append on all\nthree nodes commits lost updates (P4) and G-single, which snapshot isolation forbids:\nGalera certifies write sets by key but has no first-committer-wins over reads.",
         0, vec![]),
        ("galera-pause-append", "MDEV-38977 as Jepsen first saw it, with process pauses: galera-healthy-append while a\nrandom node's processes are paused for 5 s, then 5 s of quiet, from 10 s on.",
         0, vec!["--fault", "pause", "--fault-at", "10"]),
        ("galera-healthy-append-ser", "galera-healthy-append at SERIALIZABLE, whose locking reads the applier of a remote\nwrite set brute-force aborts: an acknowledged transaction can lose its own appends\n(rolled back mid-transaction while its later statements and COMMIT succeed), and\nG-single and internal anomalies follow (MDEV-38977's shape).",
         0, vec!["--isolation", "serializable"]),
        ("galera-single-node-append", "Control for galera-healthy-append-ser: every client on n1, SERIALIZABLE. With no\nremote write sets to apply there is nothing to brute-force abort a local transaction.",
         0, vec!["--client-nodes", "n1", "--isolation", "serializable"]),
        ("galera-ser-probe", "Diagnostic for galera-healthy-append-ser: the same workload, and after every statement\nof a multi-statement transaction the client checks @@in_transaction. A session that\nsilently left its transaction (rolled back by a brute-force abort) yet goes on to\nCOMMIT successfully is reported as transaction-ended-early.",
         0, vec!["--isolation", "serializable", "--probe-transaction"]),
        ("galera-stale-read", "MDEV-38999: no faults; clients on n1 and n3 read and append, clients on n2 only read,\nwith a 100 us network as on Jepsen's LAN. A transaction on n2 that begins after an\nappend on n1 or n3 was acknowledged does not observe it (a real-time stale read):\nn2 applies a certified write set after its origin has acknowledged the commit.",
         0, vec!["--client-nodes", "n1,n2,n3", "--read-only", "n2", "--duration", "120"]),
        ("galera-stale-read-sync-wait", "Control for galera-stale-read: wsrep_sync_wait=1 makes every read wait until its node\nhas applied everything certified before it, so no read is stale.",
         0, vec!["--client-nodes", "n1,n2,n3", "--read-only", "n2", "--duration", "120", "--sync-wait", "1"]),
        ("galera-crash-all-flush0", "MDEV-38974: power-fail all three nodes together at 20 s with\ninnodb_flush_log_at_trx_commit=0 (MariaDB's recommendation for Galera), reboot them, and\nbootstrap from the most advanced node: acknowledged appends are lost on every node.",
         0, [&["--fault", "crash"][..], FAULT].concat()),
        ("galera-crash-all-flush1", "galera-crash-all-flush0 with innodb_flush_log_at_trx_commit=1: every commit is synced,\nso far fewer (Jepsen: not zero) acknowledged appends are lost.",
         1, [&["--fault", "crash"][..], FAULT].concat()),
        ("galera-kill-all", "MDEV-38974 as Jepsen ran it: kill every node's processes together at 20 s with\ninnodb_flush_log_at_trx_commit=0, restart them and bootstrap from the most advanced node.\nThe page cache survives, but InnoDB's unflushed log buffer does not.",
         0, [&["--fault", "kill"][..], FAULT].concat()),
        ("galera-kill-all-flush1", "galera-kill-all with innodb_flush_log_at_trx_commit=1.",
         1, [&["--fault", "kill"][..], FAULT].concat()),
    ];
    let names = nodes("n", 3);
    specs
        .iter()
        .map(|(name, comment, flush, oracle)| {
            let stale = name.starts_with("galera-stale-read");
            let mut args = strings(&["galera"]);
            args.extend(strings(oracle));
            args.extend(strings(&["--label", &format!("12.1.2/{name}")]));
            Scenario {
                name: name.to_string(),
                comment: comment.to_string(),
                generated: format!(
                    "MariaDB 12.1.2, Galera 26.4.24, three nodes, innodb_flush_log_at_trx_commit={flush}{}{}",
                    if oracle.is_empty() { "" } else { ", " },
                    oracle.join(" ")
                ),
                machines: names
                    .iter()
                    .map(|n| (n.clone(), galera_node(n, *flush)))
                    .collect(),
                // The in-guest check holds the whole history: up to 200,000 transactions.
                oracle: Machine::program(&args).memory(if stale { 2048 } else { 1024 }),
                latency: if stale { "100us" } else { "1ms" }.into(),
            }
        })
        .collect()
}

/// Every committed scenario, as (path under `harness/jepsen`, contents).
pub fn all() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (system, scenarios) in [
        ("nats", nats()),
        ("etcd", etcd()),
        ("postgres", postgres()),
        ("kafka", kafka()),
        ("mysql", mysql()),
        ("galera", galera()),
    ] {
        for scenario in scenarios {
            out.push((
                format!("{system}/{}.yaml", scenario.name),
                scenario.render(),
            ));
        }
    }
    let stall = postgres_stall();
    out.push((
        format!("postgres/stall/{}.yaml", stall.name),
        stall.render(),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_render_valid_yaml_with_the_oracle_binary() {
        let all = all();
        assert!(all.len() >= 41);
        let mut names = std::collections::BTreeSet::new();
        for (path, text) in &all {
            assert!(
                text.contains("dockerfile: crates/seedbox-jepsen/guest.Dockerfile"),
                "{path}"
            );
            let name = text.lines().find_map(|l| l.strip_prefix("name: ")).unwrap();
            assert!(names.insert(name.to_string()), "duplicate {name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{name} is not a DNS label"
            );
        }
    }
}
