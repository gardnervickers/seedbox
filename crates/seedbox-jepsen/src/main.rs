//! `seedbox-jepsen`: the guest oracles and clients for Jepsen-style
//! reproductions under Seedbox, plus host-side tools (scenario generation,
//! the oracle build, log re-checks, the Elle cross-check and seed campaigns).
use clap::{Parser, Subcommand, ValueEnum};
use seedbox_jepsen::{campaign, checker, kit, scenario, summarize, systems, tools};
use std::{fs, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    version,
    about = "Jepsen-style oracles, checkers and campaign tools for Seedbox."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the NATS JetStream queue oracle (guest).
    Nats(systems::nats::Args),
    /// Run the etcd lock-set oracle (guest).
    Etcd(systems::etcd::Args),
    /// Run one etcd lock-set client (guest).
    EtcdClient(systems::etcd_client::Args),
    /// Run the etcd register (read, write, compare-and-set) oracle (guest).
    EtcdRegister(systems::etcd_register::Args),
    /// Run the etcd list-append transaction oracle (guest).
    EtcdAppend(systems::etcd_append::Args),
    /// Run the PostgreSQL list-append oracle (guest).
    Postgres(systems::postgres::Args),
    /// Report PostgreSQL backend and lock state at the given seconds (guest diagnostic).
    PgProbe {
        #[arg(long, default_value = "pg")]
        host: String,
        /// Seconds after start at which to probe; repeatable.
        #[arg(long)]
        at: Vec<f64>,
    },
    /// Run the Kafka queue oracle (guest).
    Kafka(systems::kafka::Args),
    /// Run the KRaft metadata-quorum oracle (guest).
    Kraft(systems::kraft::Args),
    /// Send one admin request to one KRaft node repeatedly and print the answers.
    KraftProbe(systems::kraft::ProbeArgs),
    /// Run the KRaft workload client (guest).
    KraftClient(systems::kraft_client::Args),
    /// Run the MySQL list-append, non-repeatable-read or MAV oracle (guest).
    Mysql(systems::mysql::Args),
    /// Run the MariaDB Galera Cluster list-append oracle (guest).
    Galera(systems::galera::Args),
    /// Run the RustFS object-store oracle (guest).
    Rustfs(systems::rustfs::Args),
    /// Run the TigerBeetle oracle (guest).
    Tigerbeetle(systems::tigerbeetle::Args),
    /// Run a TigerBeetle replica's node agent (guest).
    TbNode(systems::tigerbeetle_node::Args),
    /// Serve one TigerBeetle client session over stdin and stdout (the oracle's child).
    TbWorker(systems::tigerbeetle::WorkerArgs),
    /// Check O_DIRECT, io_uring and fallocate in a directory (guest diagnostic).
    IoProbe { dir: PathBuf },
    /// Fetch the pinned TigerBeetle C client library for this host and the guests.
    FetchTbClient {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Re-run a system's checker on a saved oracle log and print the result as JSON.
    Check {
        #[arg(value_enum)]
        system: System,
        log: PathBuf,
        /// PostgreSQL, MySQL and Galera list-append: the consistency model.
        #[arg(long, default_value = "serializable")]
        model: String,
        /// Kafka: the workload, txn or produce.
        #[arg(long, default_value = "txn")]
        workload: String,
    },
    /// Write a list-append log's history as Jepsen EDN for Elle.
    Edn {
        log: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Write the scenario projects under harness/jepsen.
    Gen {
        /// Repository root [default: this crate's checkout].
        #[arg(long)]
        root: Option<PathBuf>,
        /// Fail when a committed scenario differs instead of writing it.
        #[arg(long)]
        check: bool,
    },
    /// Build the static guest oracle binary into build/jepsen/ARCH (Docker-compatible engine).
    BuildOracle {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, default_value = "arm64")]
        arch: String,
        /// Engine socket URL [default: DOCKER_HOST].
        #[arg(long)]
        engine: Option<String>,
    },
    /// Cross-check the list-append checker against Jepsen's Elle (Docker-compatible engine).
    ElleCrossCheck {
        /// Histories as LABEL=LOG or LABEL@MODEL,MODEL=LOG.
        histories: Vec<String>,
        /// A file of LABEL=LOG lines; relative logs resolve against its directory.
        #[arg(long)]
        list: Vec<PathBuf>,
        #[arg(long, default_value = "serializable")]
        models: String,
        /// Models run on Elle only, without comparison.
        #[arg(long, default_value = "")]
        elle_only: String,
        #[arg(long)]
        engine: Option<String>,
        /// CPUs the Elle container may use.
        #[arg(long, default_value = "8,9")]
        cpus: String,
        /// Working directory for the jar, EDN files and Elle's output.
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        markdown: Option<PathBuf>,
    },
    /// Run scenarios over seeds, replay a sample, collect evidence and summarize (host).
    Campaign(campaign::Args),
    /// Summarize a `seedbox explore --json` document: a row per member and a tally (host).
    Summarize {
        /// The explore document, or `-` for standard input.
        file: PathBuf,
    },
}

#[derive(ValueEnum, Clone, Copy)]
enum System {
    /// A NATS oracle log.
    Nats,
    /// An etcd oracle log.
    Etcd,
    /// A PostgreSQL (list-append) oracle log.
    Postgres,
    /// A Kafka oracle log.
    Kafka,
    /// A KRaft oracle log (its workload comes from the log).
    Kraft,
    /// A MySQL oracle log (its workload comes from the log).
    Mysql,
    /// A Galera oracle log (stale reads and per-node loss as well).
    Galera,
}

/// The guest property each oracle's verdict evaluates.
fn check_of(command: &Command) -> Option<&'static str> {
    Some(match command {
        Command::Nats(_) => "nats: acknowledged messages survive",
        Command::Etcd(_) => "etcd: lock-protected updates are not lost",
        Command::EtcdAppend(_) => {
            "etcd: list-append transactions are serializable and every member recovers"
        }
        Command::EtcdRegister(_) => {
            "etcd: register histories are linearizable and every member recovers"
        }
        Command::Postgres(_) => {
            "postgres: transactions meet their isolation level, acknowledged commits survive and the server recovers"
        }
        Command::Kafka(_) => "kafka: the queue's checks hold",
        Command::Mysql(_) => "mysql: transactions meet their isolation level",
        Command::Galera(_) => "galera: transactions meet their isolation level",
        Command::Rustfs(_) => "rustfs: acknowledged objects survive and every server recovers",
        Command::Tigerbeetle(_) => "tigerbeetle: the ledger's checks hold",
        _ => return None,
    })
}

fn main() -> ExitCode {
    let command = Cli::parse().command;
    let oracle = check_of(&command);
    if let Some(check) = oracle {
        kit::declare_check(check);
    }
    let code = match command {
        Command::Nats(args) => systems::nats::run(args),
        Command::Etcd(args) => systems::etcd::run(args),
        Command::EtcdClient(args) => systems::etcd_client::run(args),
        Command::EtcdRegister(args) => systems::etcd_register::run(args),
        Command::EtcdAppend(args) => systems::etcd_append::run(args),
        Command::Postgres(args) => systems::postgres::run(args),
        Command::PgProbe { host, at } => systems::postgres::probe(&host, &at),
        Command::Kafka(args) => systems::kafka::run(args),
        Command::Kraft(args) => systems::kraft::run(args),
        Command::KraftClient(args) => systems::kraft_client::run(args),
        Command::KraftProbe(args) => systems::kraft::probe(args),
        Command::Mysql(args) => systems::mysql::run(args),
        Command::Galera(args) => systems::galera::run(args),
        Command::Rustfs(args) => systems::rustfs::run(args),
        Command::Tigerbeetle(args) => systems::tigerbeetle::run(args),
        Command::TbNode(args) => systems::tigerbeetle_node::run(args),
        Command::TbWorker(args) => systems::tigerbeetle::worker(args),
        Command::IoProbe { dir } => {
            println!("{}", systems::tigerbeetle_node::probe(&dir));
            0
        }
        Command::FetchTbClient { root } => {
            let root = root.unwrap_or_else(tools::default_root);
            let mut code = 0;
            for triple in tools::TB_CLIENT_TRIPLES {
                match tools::tb_client_lib(&root, triple) {
                    Ok(paths) => {
                        for path in paths {
                            println!("FETCHED: {}", path.display());
                        }
                    }
                    Err(error) => code = fail(&error),
                }
            }
            code
        }
        Command::Check {
            system,
            log,
            model,
            workload,
        } => check(system, &log, &model, &workload),
        Command::Edn { log, output } => edn(&log, output.as_deref()),
        Command::Gen { root, check } => generate(root.unwrap_or_else(tools::default_root), check),
        Command::BuildOracle { root, arch, engine } => {
            match tools::build_oracle(
                &root.unwrap_or_else(tools::default_root),
                &arch,
                engine.as_deref(),
            ) {
                Ok(path) => {
                    println!("BUILT: {}", path.display());
                    0
                }
                Err(error) => fail(&error),
            }
        }
        Command::ElleCrossCheck {
            histories,
            list,
            models,
            elle_only,
            engine,
            cpus,
            work,
            out,
            markdown,
        } => cross_check(
            histories,
            list,
            &models,
            &elle_only,
            engine.as_deref(),
            &cpus,
            &work,
            out,
            markdown,
        ),
        Command::Campaign(args) => campaign::run(args),
        Command::Summarize { file } => summarize::run(&file),
    };
    // Under Seedbox an inconclusive oracle leaves its property unevaluated
    // rather than failing: it is not a finding.
    let code = if oracle.is_some() && code == kit::INCONCLUSIVE && kit::under_seedbox() {
        kit::OK
    } else {
        code
    };
    ExitCode::from(code)
}

fn fail(message: &str) -> u8 {
    eprintln!("error: {message}");
    2
}

fn read(path: &std::path::Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn check(system: System, log: &std::path::Path, model: &str, workload: &str) -> u8 {
    let text = match read(log) {
        Ok(text) => text,
        Err(error) => return fail(&error),
    };
    let (code, result) = match system {
        System::Nats => systems::nats::recheck(&text),
        System::Etcd => systems::etcd::recheck(&text),
        System::Kafka => systems::kafka::recheck(&text, workload),
        System::Kraft => systems::kraft::recheck(&text),
        System::Postgres => match systems::postgres::recheck(&text, model) {
            Ok(result) => result,
            Err(error) => return fail(&error),
        },
        System::Mysql => match systems::mysql::recheck(&text, model) {
            Ok(result) => result,
            Err(error) => return fail(&error),
        },
        System::Galera => match systems::galera::recheck(&text, model) {
            Ok(result) => result,
            Err(error) => return fail(&error),
        },
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&result).unwrap_or_default()
    );
    code
}

fn edn(log: &std::path::Path, output: Option<&std::path::Path>) -> u8 {
    let txns = match read(log)
        .and_then(|text| checker::elle::from_h_lines(&kit::tagged_lines(&text, "H ")))
    {
        Ok(txns) => txns,
        Err(error) => return fail(&error),
    };
    let text = checker::edn::lines(&txns).join("\n") + "\n";
    match output {
        Some(path) => match fs::write(path, text) {
            Ok(()) => 0,
            Err(error) => fail(&format!("{}: {error}", path.display())),
        },
        None => {
            print!("{text}");
            0
        }
    }
}

fn generate(root: PathBuf, check: bool) -> u8 {
    let mut stale = Vec::new();
    for (path, text) in scenario::all() {
        let target = root.join("harness/jepsen").join(&path);
        if check {
            if fs::read_to_string(&target).ok().as_deref() != Some(text.as_str()) {
                stale.push(path);
            }
            continue;
        }
        if let Err(error) =
            fs::create_dir_all(target.parent().unwrap()).and_then(|()| fs::write(&target, text))
        {
            return fail(&format!("{}: {error}", target.display()));
        }
    }
    if !stale.is_empty() {
        return fail(&format!(
            "scenarios differ from the generator: {}; run seedbox-jepsen gen",
            stale.join(", ")
        ));
    }
    0
}

#[allow(clippy::too_many_arguments)]
fn cross_check(
    histories: Vec<String>,
    lists: Vec<PathBuf>,
    models: &str,
    elle_only: &str,
    engine: Option<&str>,
    cpus: &str,
    work: &std::path::Path,
    out: Option<PathBuf>,
    markdown: Option<PathBuf>,
) -> u8 {
    let models: Vec<String> = models
        .split(',')
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .collect();
    let elle_only: Vec<String> = elle_only
        .split(',')
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .collect();
    let mut plans = Vec::new();
    let here = std::env::current_dir().unwrap_or_default();
    for item in &histories {
        match tools::parse_history(item, &models, &here) {
            Ok(plan) => plans.push(plan),
            Err(error) => return fail(&error),
        }
    }
    for list in &lists {
        let base = list.parent().map(|p| p.join("..")).unwrap_or_default();
        let text = match read(list) {
            Ok(text) => text,
            Err(error) => return fail(&error),
        };
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            match tools::parse_history(line, &models, &base) {
                Ok(plan) => plans.push(plan),
                Err(error) => return fail(&error),
            }
        }
    }
    match tools::elle_cross_check(&plans, &elle_only, engine, cpus, work) {
        Ok((rows, agree)) => {
            let table = tools::cross_check_table(&rows);
            println!("{table}");
            if let Some(path) = markdown {
                let _ = fs::write(path, format!("{table}\n"));
            }
            if let Some(path) = out {
                let _ = fs::write(
                    path,
                    serde_json::to_string_pretty(&rows).unwrap_or_default(),
                );
            }
            u8::from(!agree)
        }
        Err(error) => fail(&error),
    }
}
