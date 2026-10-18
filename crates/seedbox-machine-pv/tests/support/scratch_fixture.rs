//! Host-only process fixture for named PV boot/capture/restore files. No native VM.
//! An explicit flag is mandatory; Cargo bin-target invocations are benign.
mod policy_fixture;
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
fn argument(args: &[OsString], flag: &str) -> Option<OsString> {
    args.windows(2).find(|p| p[0] == flag).map(|p| p[1].clone())
}
fn log(path: &Path, event: Value) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &event)?;
    writeln!(file)
}
fn receive(input: &mut impl Read) -> io::Result<Option<Value>> {
    let mut first = [0];
    if input.read(&mut first)? == 0 {
        return Ok(None);
    }
    let mut header = [0; 4];
    header[0] = first[0];
    input.read_exact(&mut header[1..])?;
    let length = u32::from_le_bytes(header) as usize;
    assert!(length <= 8 << 20);
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}
fn send(output: &mut impl Write, message: Value) -> io::Result<()> {
    let body = serde_json::to_vec(&message)?;
    output.write_all(&(body.len() as u32).to_le_bytes())?;
    output.write_all(&body)?;
    output.flush()
}
fn run(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    assert!(args.iter().any(|a| a == "--seedbox-pv-worker-v1"));
    let record = PathBuf::from(argument(args, "--scratch-record").unwrap());
    let mode = argument(args, "--scratch-mode").unwrap_or_else(|| "ok".into());
    log(&record, json!({"kind":"start","pid":std::process::id()}))?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut root = None::<PathBuf>;
    let mut overlays = Vec::new();
    let mut now = 0;
    let mut policy = Value::Null;
    let mut memory = 0;
    let mut native_chain = Value::Null;
    while let Some(command) = receive(&mut input)? {
        log(&record, json!({"kind":"command", "command":command}))?;
        let response = if let Some(boot) = command.get("Boot") {
            native_chain = boot["witness"]["initial"].clone();
            let kernel = PathBuf::from(boot["kernel"].as_str().unwrap());
            let directory = kernel.parent().unwrap().to_owned();
            assert!(fs::read(&kernel)?.len() >= 64);
            assert!(!fs::read(boot["initrd"].as_str().unwrap())?.is_empty());
            if let Some(disk) = boot.get("disk").filter(|v| !v.is_null()) {
                if let Some(base) = disk["base"].as_str() {
                    assert!(Path::new(base).starts_with(&directory));
                    assert_eq!(
                        fs::metadata(base)?.len(),
                        disk["sectors"].as_u64().unwrap() * 512
                    );
                }
                overlays = match disk["overlay"].as_str() {
                    Some(path) => {
                        assert!(Path::new(path).starts_with(&directory));
                        fs::read(path)?
                    }
                    None => Vec::new(),
                };
            }
            if let Some(path) = boot["restore"].as_str() {
                assert!(Path::new(path).starts_with(&directory));
                let saved: Value = serde_json::from_slice(&fs::read(path)?)?;
                assert_eq!(saved["fixture"], "pv-scratch-NOT-native");
                overlays = serde_json::from_value(saved["overlays"].clone())?;
            }
            root = Some(directory);
            policy = boot.get("policy").cloned().unwrap_or(Value::Null);
            memory = boot["memory_bytes"].as_u64().unwrap();
            now = boot["now"].as_u64().unwrap();
            if mode == "boot-error" {
                json!({"Error":"scratch fixture boot error"})
            } else {
                let mut response = json!({"Ready":{"witness":boot["witness"],"memory_bytes":boot["memory_bytes"],"layout_version":boot["layout_version"]}});
                if !policy.is_null() {
                    response["Ready"]["policy"] = policy.clone();
                }
                response
            }
        } else if let Some(run) = command.get("Run") {
            for _ in 0..run["frames"].as_u64().unwrap() {
                let mut length = [0; 2];
                input.read_exact(&mut length)?;
                let mut bytes = vec![0; usize::from(u16::from_le_bytes(length))];
                input.read_exact(&mut bytes)?;
            }
            let mut end = [0; 2];
            input.read_exact(&mut end)?;
            assert_eq!(end, [0; 2]);
            assert_eq!(
                run["now"], run["upto"],
                "fixture accepts only zero-budget host admissions"
            );
            now = run["upto"].as_u64().unwrap();
            send(
                &mut output,
                json!({"Stopped":{"time":now,"stop":match mode.to_string_lossy().as_ref(){"powered-off"|"psci-powered-off"=>"PoweredOff","reset"|"triplefault"=>"Reset",_=>"Horizon"},"writes":0}}),
            )?;
            output.write_all(&[255])?;
            output.flush()?;
            continue;
        } else if command.as_str() == Some("ExecutionEvidence") {
            json!({"ExecutionEvidence":{"chain":native_chain,"pending":[0]}})
        } else if let Some(request) = command.get("Evidence") {
            assert!(!policy.is_null());
            policy_fixture::evidence(
                Path::new(request["pages"].as_str().unwrap()),
                root.as_ref().unwrap(),
                memory,
            )?
        } else if let Some(path) = command.get("Capture").and_then(Value::as_str) {
            assert!(Path::new(path).starts_with(root.as_ref().unwrap()));
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            serde_json::to_writer(
                &mut file,
                &json!({"fixture":"pv-scratch-NOT-native","time":now,"overlays":overlays}),
            )?;
            if mode == "capture-error" {
                json!({"Error":"scratch fixture capture error"})
            } else {
                json!("Captured")
            }
        } else if let Some(path) = command.get("ExportDisk").and_then(Value::as_str) {
            assert!(Path::new(path).starts_with(root.as_ref().unwrap()));
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?
                .write_all(&overlays)?;
            if mode == "export-error" {
                json!({"Error":"scratch fixture export error"})
            } else {
                json!("Exported")
            }
        } else {
            panic!("unexpected PV fixture command {command}");
        };
        send(&mut output, response)?;
    }
    Ok(())
}
fn owner_probe(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine_pv::machine::pv::{PVMachine, Snapshot};
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
    let machine = PVMachine::from_snapshot_in(Path::new("/never-launched"), &saved, &root)
        .map_err(io::Error::other)?;
    fs::write(marker, b"cold scratch owner ready")?;
    loop {
        std::hint::black_box(&machine);
        // Retain the cold machine until external termination, without periodic timer wakeups.
        std::thread::park();
    }
}

fn encoding_probe(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    use seedbox_machine_pv::machine::pv::{PVMachine, Snapshot};
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
        Some(root) => PVMachine::from_snapshot_in(Path::new("/never-launched"), &saved, &root)
            .map_err(io::Error::other),
        None => {
            PVMachine::from_snapshot(Path::new("/never-launched"), &saved).map_err(io::Error::other)
        }
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
