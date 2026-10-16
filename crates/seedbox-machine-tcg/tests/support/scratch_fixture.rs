//! Explicit, host-only QMP/RSP process fixture. Never starts QEMU or a guest.
//! No fixture flag => no protocol I/O (also safe as a Cargo bin test target).
mod policy_fixture;
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::{fd::FromRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};
fn argument(args: &[OsString], flag: &str) -> Option<OsString> {
    args.windows(2).find(|p| p[0] == flag).map(|p| p[1].clone())
}
fn log(path: &Path, event: Value) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &event)?;
    writeln!(file)
}
fn response(stream: &mut UnixStream, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, &value)?;
    writeln!(stream)
}
fn file_address(command: &Value, boot: &Path) -> io::Result<PathBuf> {
    assert!(
        command["arguments"].get("uri").is_none(),
        "no ambiguous file URI"
    );
    let channels = command["arguments"]["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0]["channel-type"], "main");
    let addr = &channels[0]["addr"];
    assert_eq!(addr["transport"], "file");
    assert_eq!(addr["offset"], 0);
    let path = PathBuf::from(addr["filename"].as_str().unwrap());
    assert!(path.starts_with(boot), "migration escaped boot scratch");
    Ok(path)
}
fn rsp(mut stream: UnixStream, bad: bool) -> io::Result<()> {
    loop {
        let mut one = [0];
        stream.read_exact(&mut one)?;
        if one[0] == b'+' {
            continue;
        }
        assert_eq!(one[0], b'$');
        let mut payload = Vec::new();
        loop {
            stream.read_exact(&mut one)?;
            if one[0] == b'#' {
                break;
            }
            payload.push(one[0]);
            assert!(payload.len() < 4096);
        }
        let mut checksum = [0; 2];
        stream.read_exact(&mut checksum)?;
        assert_eq!(payload, b"qAttached");
        stream.write_all(if bad { b"+$0#30" } else { b"+$1#31" })?;
    }
}
fn run(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let record = PathBuf::from(argument(args, "--scratch-record").unwrap());
    let mode = argument(args, "--scratch-mode").unwrap_or_else(|| "ok".into());
    let boot = PathBuf::from(argument(args, "-kernel").unwrap())
        .parent()
        .unwrap()
        .to_owned();
    assert!(args.iter().any(|a| a == "-nodefaults"));
    assert_eq!(argument(args, "-monitor").unwrap(), "none");
    assert!(!args.iter().any(|a| a == "-mon"));
    assert_eq!(argument(args, "-qmp").unwrap(), "chardev:seedbox-qmp");
    let chardev: Vec<_> = args
        .windows(2)
        .filter(|p| p[0] == "-chardev")
        .map(|p| p[1].to_str().unwrap())
        .collect();
    assert!(chardev.contains(&"socket,id=seedbox-qmp,fd=0"));
    let gdb = args.iter().any(|a| a == "-gdb");
    if gdb {
        assert!(chardev.contains(&"socket,id=seedbox-gdb,fd=1"));
        assert_eq!(argument(args, "-gdb").unwrap(), "chardev:seedbox-gdb");
    }
    log(
        &record,
        json!({"kind":"start", "pid":std::process::id(), "boot":boot, "args":args}),
    )?;
    if mode == "early-exit" {
        eprintln!("scratch fixture early exit");
        std::process::exit(42);
    }
    // These are the explicitly supplied child stdio descriptors, owned only by
    // this fixture. No unsafe descriptor manipulation exists in production.
    let mut socket = unsafe { UnixStream::from_raw_fd(0) };
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;
    if gdb {
        let stream = unsafe { UnixStream::from_raw_fd(1) };
        let bad = mode == "bad-gdb";
        std::thread::spawn(move || {
            let _ = rsp(stream, bad);
        });
    }
    response(&mut socket, json!({"QMP":{"fixture":true}}))?;
    let mut input = BufReader::new(socket.try_clone()?);
    let mut image = b"opaque host fixture, NOT a QEMU migration".to_vec();
    let mut now = 0;
    let mut migrating = false;
    let sectors = args.windows(2).find_map(|p| {
        if p[0] != "-device" {
            return None;
        }
        p[1].to_str()?
            .strip_prefix("seedbox-blk-device,sectors=")?
            .split(',')
            .next()?
            .parse::<u64>()
            .ok()
    });
    loop {
        let mut line = String::new();
        if input.by_ref().take(8 << 20).read_line(&mut line)? == 0 {
            // QEMU keeps running when QMP disconnects. Require the parent's
            // explicit kill+wait instead of making EOF itself clean up for it.
            loop {
                std::thread::park_timeout(Duration::from_secs(60));
            }
        }
        assert!(line.ends_with('\n'));
        let command: Value = serde_json::from_str(&line)?;
        log(&record, json!({"kind":"command", "command":command}))?;
        let arguments = &command["arguments"];
        let result = match command["execute"].as_str().unwrap() {
            "qmp_capabilities" | "migrate_cancel" => json!({}),
            "query-seedbox-run" => json!({"time":now,"reason":"boundary"}),
            "migrate-incoming" => {
                image = fs::read(file_address(&command, &boot)?)?;
                json!({})
            }
            "migrate" => {
                let path = file_address(&command, &boot)?;
                fs::write(path, &image)?;
                migrating = true;
                json!({})
            }
            "query-migrate" => {
                json!({"status":if mode == "capture-error" {"failed"} else {"completed"}})
            }
            "query-status" => json!({"status":if migrating {"postmigrate"} else {"paused"}}),
            "seedbox-capture-complete" => {
                migrating = false;
                json!({})
            }
            "seedbox-advance" => {
                now = arguments["time"].as_u64().unwrap();
                json!({})
            }
            "seedbox-run" => {
                assert_eq!(
                    arguments["budget"], 0,
                    "host fixture accepts only zero-budget admissions"
                );
                now = arguments["time"].as_u64().unwrap() + arguments["budget"].as_u64().unwrap();
                response(&mut socket, json!({"event":"STOP"}))?;
                json!({})
            }
            "seedbox-drain-lifecycle" => json!([]),
            "seedbox-input-lifecycle" | "seedbox-net-receive" => json!({"runnable":false}),
            "seedbox-net-drain" => json!({"frames":[],"more":false}),
            "seedbox-disk-export" => {
                let path = PathBuf::from(arguments["path"].as_str().unwrap());
                assert!(path.starts_with(&boot));
                let mut body = b"SBXSTOR1".to_vec();
                body.extend_from_slice(&sectors.unwrap().to_le_bytes());
                body.extend_from_slice(&[0; 16]);
                fs::write(path, body)?;
                json!({})
            }
            other => policy_fixture::handle(&command, &boot, &mode.to_string_lossy())
                .unwrap_or_else(|| panic!("unexpected host fixture QMP command {other}")),
        };
        response(&mut socket, json!({"return":result}))?;
    }
}
fn owner_probe(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine_tcg::machine::tcg::{Snapshot, TcgMachine};
    let root = PathBuf::from(argument(args, "--scratch-root").unwrap());
    let snapshot = PathBuf::from(argument(args, "--scratch-snapshot").unwrap());
    let marker = PathBuf::from(argument(args, "--scratch-ready").unwrap());
    let limits = seedbox_machine::snapshot::DecodeLimits {
        encoded_bytes: 8_000_000,
        allocation_bytes: 16_000_000,
        collection_items: 100_000,
        total_items: 300_000,
        string_bytes: 4096,
        blob_bytes: 4_000_000,
    };
    let saved = Snapshot::read_from(&mut fs::File::open(snapshot)?, &limits)?;
    // Only cold snapshots are supplied: no VM, child worker or RAM allocation.
    let machine = TcgMachine::from_snapshot_in(Path::new("/never-launched"), &saved, &root)?;
    fs::write(marker, b"cold scratch owner ready")?;
    loop {
        std::hint::black_box(&machine);
        std::thread::park_timeout(std::time::Duration::from_secs(60));
    }
}

fn encoding_probe(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine_tcg::machine::tcg::{Snapshot, TcgMachine};
    let root = argument(args, "--scratch-root").map(PathBuf::from);
    let artifact = PathBuf::from(argument(args, "--scratch-snapshot").unwrap());
    let result_path = PathBuf::from(argument(args, "--scratch-result").unwrap());
    let expect_ok = args.iter().any(|a| a == "--scratch-expect-ok");
    // One-shot process-global override only inside this explicitly selected child.
    if let Some(root) = argument(args, "--scratch-temp-override") {
        tempfile::env::override_temp_dir(Path::new(&root))
            .expect("fresh isolated fixture must not have an existing override");
    }
    let limits = seedbox_machine::snapshot::DecodeLimits {
        encoded_bytes: 8_000_000,
        allocation_bytes: 16_000_000,
        collection_items: 100_000,
        total_items: 300_000,
        string_bytes: 4096,
        blob_bytes: 4_000_000,
    };
    let saved = Snapshot::read_from(&mut fs::File::open(artifact)?, &limits)?;
    let result = match root {
        Some(root) => TcgMachine::from_snapshot_in(Path::new("/never-launched"), &saved, &root),
        None => TcgMachine::from_snapshot(Path::new("/never-launched"), &saved),
    };
    let message = match result {
        Ok(machine) => {
            if let Some(parent) = argument(args, "--scratch-expected-parent") {
                assert_eq!(fs::read_dir(parent)?.count(), 2, "selected authority must hold its unique child alongside the caller marker while the machine lives");
            }
            drop(machine);
            assert!(expect_ok, "non-UTF-8 effective root was accepted");
            "ok".to_owned()
        }
        Err(error) => {
            assert!(!expect_ok, "valid UTF-8 root rejected: {error}");
            let message = error.to_string();
            assert!(
                message.contains("UTF-8") && message.contains("choose"),
                "{message}"
            );
            message
        }
    };
    fs::write(result_path, message)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<_> = std::env::args_os().collect();
    policy_fixture::arguments(&mut args)?;
    if !args.iter().any(|a| a == "--scratch-fixture") {
        return Ok(());
    }
    if args.iter().any(|a| a == "--scratch-owner-probe") {
        return owner_probe(&args);
    }
    if args.iter().any(|a| a == "--scratch-encoding-probe") {
        return encoding_probe(&args);
    }
    run(&args)
}
