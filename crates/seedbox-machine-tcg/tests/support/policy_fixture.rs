//! Synthetic host-only QMP replies for the real Rust policy/evidence adapter.
//! Does not implement QEMU, execute an instruction or certify native RAM.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
/// Explicit policy fixture entry: immutable executable inode plus data sidecar
/// avoids a fork inheriting another test's write-open executable (ETXTBSY).
pub(super) fn arguments(args: &mut Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let executable = PathBuf::from(args.first().ok_or("missing fixture argv0")?);
    if !executable
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("policy-fixture-"))
    {
        return Ok(());
    }
    let config: Value = serde_json::from_slice(&fs::read(executable.with_extension("config"))?)?;
    args.extend([
        "--scratch-fixture".into(),
        "--scratch-mode".into(),
        config["mode"].as_str().ok_or("fixture mode")?.into(),
        "--scratch-record".into(),
        config["record"].as_str().ok_or("fixture record")?.into(),
    ]);
    Ok(())
}

use std::{
    fs,
    path::{Path, PathBuf},
};
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(super) fn handle(command: &Value, boot: &Path, mode: &str) -> Option<Value> {
    match command["execute"].as_str()? {
        "seedbox-witness-start" => {
            let arguments = command["arguments"].as_object().unwrap();
            assert_eq!(arguments.len(), 3);
            let chain = arguments["chain"].as_str().unwrap();
            assert_eq!(chain.len(), 64);
            assert!(chain
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            assert!(arguments["boot"].as_u64().is_some());
            assert!(arguments["forced"].as_bool().is_some());
            Some(json!({}))
        }
        // Match the production QMP object shapes and empty pending encodings
        // in seedbox-witness.patch. This fixed chain is explicitly synthetic:
        // the zero-budget host fixture does not model guest native execution.
        "seedbox-witness-chain" => Some(json!({
            "chain": hex(&Sha256::digest(b"synthetic host-only interaction chain"))
        })),
        "seedbox-lifecycle-pending" => Some(json!({"bytes":"lifecycle-v1:0:0::0:"})),
        "seedbox-net-pending" => Some(json!({"bytes":"net-v1:|"})),
        "seedbox-timer-policy" => Some(if mode == "bad-timer-ack" {
            json!({"unexpected":true})
        } else {
            json!({})
        }),
        "seedbox-x86-evidence" => {
            let path = PathBuf::from(command["arguments"]["page-file"].as_str().unwrap());
            assert!(path.starts_with(boot));
            let page = Sha256::digest([0x42; 4096]);
            fs::write(
                &path,
                if mode == "bad-page-file" {
                    &page[..7]
                } else {
                    &page[..8]
                },
            )
            .unwrap();
            let mut layout = Sha256::new();
            layout.update(0u64.to_le_bytes());
            layout.update(4096u64.to_le_bytes());
            let hash = hex(&Sha256::digest(b"synthetic host-only native part"));
            let mut value = json!({"regs":hash,"vectors":hash,"events":hash,"msrs":hash,"timers":hash,"timer-policy":hash,"ram":hex(&page),"ram-regions":hex(&layout.finalize()),"regions":[{"base":0,"length":4096}],"page-bytes":8});
            if mode == "bad-evidence-shape" {
                value["unowned-field"] = json!(0);
            }
            Some(value)
        }
        _ => None,
    }
}
