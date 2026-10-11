//! Portable contracts of the retained experimental tools; no QEMU is started.
mod support;
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};
use support::{json as result, python_api};

fn events() -> Vec<Value> {
    let mut events = vec![
        json!({"kind":"entropy","phase":"before_seed","count":-1,"errno":11}),
        json!({"kind":"entropy","phase":"payload","count":32,"errno":0,"bytes":"ab".repeat(32),"at_random":"cd".repeat(16)}),
        json!({"kind":"cpu","rdrand":0,"rdseed":0}),
        json!({"kind":"start"}),
    ];
    for round in 0..3 {
        events.extend([json!({"kind":"cpu_work","round":round,"before_ns":1,"after_ns":2,"tsc_before":1,"tsc_after":2}),json!({"kind":"sleep","round":round,"before_ns":1,"after_ns":1+(round+1)*10_000_000,"requested_ns":(round+1)*10_000_000})]);
    }
    events.extend([
        json!({"kind":"poll","count":123,"before_ns":1,"after_ns":1_000_001}),
        json!({"kind":"result","status":"PASS","reason":"completed"}),
    ]);
    events
}
fn console(events: &[Value]) -> String {
    format!(
        "kernel log\r\n{}",
        events
            .iter()
            .map(|event| format!("SBX {event}\r\n"))
            .collect::<String>()
    )
}
#[test]
fn input_probe_classification_requires_complete_guest_evidence_and_keeps_timeout_precedence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("console");
    let classify = |text: &str, code: i32, timeout: bool| {
        fs::write(&path, text).unwrap();
        result(&python_api(
            "tools/mac-tcg/run.py",
            &format!(
                "subject.classify(Path(sys.argv[2]).read_bytes(), {code}, {})",
                if timeout { "True" } else { "False" }
            ),
            &[&path],
        ))[0]
            .clone()
    };
    let complete = events();
    let good = console(&complete);
    assert_eq!(classify("reboot: Power down\n", 0, false), "INCOMPLETE");
    assert_eq!(classify(&good, 0, false), "PASS");
    for (suffix, code, timeout, expected) in [
        ("", 1, false, "ERROR"),
        ("", -9, true, "TIMEOUT"),
        ("SBX {", -9, true, "TIMEOUT"),
        ("SBX invalid\n", 0, false, "ERROR"),
    ] {
        assert_eq!(classify(&(good.clone() + suffix), code, timeout), expected);
    }
    let mut missing = complete.clone();
    missing.remove(4);
    assert_eq!(classify(&console(&missing), 0, false), "ERROR");
    let mut duplicate = complete.clone();
    duplicate.push(duplicate.last().unwrap().clone());
    assert_eq!(classify(&console(&duplicate), 0, false), "ERROR");
    for (index, field, value) in [
        (1, "count", json!(-1)),
        (1, "bytes", json!("00")),
        (2, "rdrand", json!(1)),
        (4, "after_ns", json!(1)),
        (5, "after_ns", json!(3)),
        (10, "after_ns", json!(3)),
    ] {
        let mut bad = complete.clone();
        bad[index][field] = value;
        assert_eq!(classify(&console(&bad), 0, false), "ERROR");
    }
    let mut failed = complete;
    *failed.last_mut().unwrap() =
        json!({"kind":"result","status":"FAIL","reason":"planted_failure"});
    assert_eq!(classify(&console(&failed), 0, false), "FAIL");
}

#[test]
fn experimental_archive_has_canonical_metadata_and_domain_separated_seed_bytes() {
    let archive = |control: &str| {
        hex::decode(
            result(&python_api(
                "tools/mac-tcg/run.py",
                &format!("subject.initramfs(b'ELF probe', 42, '{control}').hex()"),
                &[],
            ))
            .as_str()
            .unwrap(),
        )
        .unwrap()
    };
    let bytes = archive("none");
    assert_eq!(bytes, archive("none"));
    assert_ne!(bytes, archive("no-entropy"));
    let mut entries = std::collections::BTreeMap::new();
    let mut offset = 0;
    loop {
        assert_eq!(&bytes[offset..offset + 6], b"070701");
        let fields: Vec<usize> = (0..13)
            .map(|i| {
                usize::from_str_radix(
                    std::str::from_utf8(&bytes[offset + 6 + i * 8..offset + 14 + i * 8]).unwrap(),
                    16,
                )
                .unwrap()
            })
            .collect();
        offset += 110;
        let name = std::str::from_utf8(&bytes[offset..offset + fields[11] - 1])
            .unwrap()
            .to_owned();
        offset = (offset + fields[11] + 3) & !3;
        let body = bytes[offset..offset + fields[6]].to_vec();
        offset = (offset + fields[6] + 3) & !3;
        assert_eq!(&fields[2..4], &[0, 0]);
        assert_eq!(fields[5], 0);
        if name == "TRAILER!!!" {
            break;
        }
        entries.insert(name, (fields[1], body));
    }
    assert_eq!(entries["init"], (0o100755, b"ELF probe".to_vec()));
    let seed = |number, domain| {
        hex::decode(
            result(&python_api(
                "tools/mac-tcg/run.py",
                &format!("subject.seed_bytes({number}, '{domain}').hex()"),
                &[],
            ))
            .as_str()
            .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(entries["seed"].1, seed(42, "linux-pool"));
    assert_ne!(seed(42, "linux-pool"), seed(43, "linux-pool"));
    assert_ne!(seed(42, "linux-pool"), seed(42, "qemu"));
    assert_eq!(result(&python_api("tools/mac-tcg/service.py","{key: value.hex() for key, value in subject.contents(subject.archive([('modules', 0o040755, b''), ('init', 0o100755, b'executable\\0bytes')])).items()}",&[])),json!({"modules":"","init":hex::encode(b"executable\0bytes")}));
}

fn management(queued: bool) -> String {
    let requests = ["partition n1 n2", "heal n1 n2", "stop n2", "start n2"];
    let acks = [
        "partition n1 n2",
        "heal n1 n2",
        "stop n2 stopped",
        "start n2 launched",
    ];
    let mut text = "DROP_COUNTER_BEGIN\n(dropped 2, overlimits 0)\nDROP_COUNTER_END\n".to_owned();
    if queued {
        for (i, request) in requests.iter().enumerate() {
            text += &format!(
                "REQUEST {request} target_ns={} guest_ns={}\n",
                100 + i,
                100 + i
            );
        }
        for (i, ack) in acks.iter().enumerate() {
            text += &format!("FAULT {ack} guest_ns={}\n", 200 + i);
        }
    } else {
        for (i, (request, ack)) in requests.iter().zip(acks).enumerate() {
            text += &format!(
                "REQUEST {request} target_ns={} guest_ns={}\nFAULT {ack} guest_ns={}\n",
                100 + i * 10,
                101 + i * 10,
                102 + i * 10
            );
        }
    }
    text
}
#[test]
fn service_verdict_requires_exit_binding_fault_acknowledgements_and_live_drops() {
    let directory = tempfile::tempdir().unwrap();
    let console = directory.path().join("console");
    let mgmt = directory.path().join("mgmt");
    let classify = |text: &str, management: &str, code: i32, timeout: bool, mode: &str| {
        fs::write(&console, text).unwrap();
        fs::write(&mgmt, management).unwrap();
        result(&python_api("tools/mac-tcg/service.py",&format!("subject.classify(Path(sys.argv[2]).read_text(),Path(sys.argv[3]).read_text(),{code},{},'{mode}')",if timeout {"True"} else {"False"}),&[&console,&mgmt]))[0].clone()
    };
    let passed = "RESULT pass acked=128 failed=0\nINIT exit=0\n";
    assert_eq!(classify(passed, "", 1, false, "healthy"), "PASS");
    assert_eq!(classify(passed, "", 0, false, "healthy"), "ERROR");
    assert_eq!(
        classify("INIT exit=0\n", "", 1, false, "healthy"),
        "INCOMPLETE"
    );
    let incomplete = "RESULT incomplete acked=12 target=128\nINIT exit=4\n";
    assert_eq!(classify(incomplete, "", 9, false, "healthy"), "INCOMPLETE");
    assert_eq!(classify(incomplete, "", -9, true, "healthy"), "TIMEOUT");
    assert_eq!(
        classify(
            "RESULT fail missing k/99/000000\nINIT exit=1\n",
            "",
            3,
            false,
            "negative"
        ),
        "FAIL"
    );
    let evidence = management(false);
    assert_eq!(classify(passed, &evidence, 1, false, "fault"), "PASS");
    for bad in [String::new(), evidence.replace("dropped 2", "dropped 0")] {
        assert_eq!(classify(passed, &bad, 1, false, "fault"), "INCOMPLETE");
    }
    for bad in [
        evidence.replace("guest_ns=102", "guest_ns=99"),
        evidence.replace("guest_ns=102", "guest_ns=100"),
        {
            let mut lines = evidence.lines().collect::<Vec<_>>();
            lines.swap(3, 4);
            lines.join("\n")
        },
    ] {
        assert_eq!(classify(passed, &bad, 1, false, "fault"), "ERROR");
    }
    let queued = management(true);
    assert_eq!(classify(passed, &queued, 1, false, "fault"), "PASS");
    assert_eq!(
        classify(
            passed,
            &queued.replace("guest_ns=200", "guest_ns=102"),
            1,
            false,
            "fault"
        ),
        "PASS"
    );
    let adapted = result(&python_api(
        "tools/mac-tcg/service.py",
        "subject.fixture('fault').decode()",
        &[],
    ));
    let adapted = adapted.as_str().unwrap();
    for text in [
        ". /bin/network.sh",
        ". /bin/members.sh",
        "< /run/seedbox-control",
        "/tmp/scenario.done",
    ] {
        assert!(adapted.contains(text));
    }
    assert!(!adapted.contains("< /dev/ttyS1"));
    let scenario = |seed| {
        result(&python_api(
            "tools/mac-tcg/service.py",
            &format!("[subject.scenario({seed})[0].decode(),subject.scenario({seed})[1]]"),
            &[],
        ))
    };
    let first = scenario(1009);
    assert_eq!(first, scenario(1009));
    assert_ne!(first[1], scenario(1010)[1]);
    let times = first[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event[0].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(times.windows(2).all(|pair| pair[0] <= pair[1]));
}

fn copy_batch(root: &Path) -> std::path::PathBuf {
    let source = support::root().join("tools/mac-tcg/testdata/service-qualified-fault");
    let batch = root.join("service-qualified-fault");
    fs::create_dir(&batch).unwrap();
    for suffix in ["console", "mgmt", "witness.json", "manifest.json"] {
        let name = format!("s1009-r0.{suffix}");
        fs::copy(source.join(&name), batch.join(name)).unwrap();
    }
    let rows: Value = serde_json::from_slice(&fs::read(source.join("runs.json")).unwrap()).unwrap();
    fs::write(batch.join("runs.json"), json!([rows[0]]).to_string()).unwrap();
    batch
}
fn update(path: &Path, change: impl FnOnce(&mut Value)) {
    let mut value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    change(&mut value);
    fs::write(path, value.to_string()).unwrap();
}
fn verify_batch(batch: &Path) -> std::process::Output {
    python_api(
        "tools/mac-tcg/service_audit.py",
        "subject.verify_batch(Path(sys.argv[2]),mode='fault',witness_required=True)",
        &[batch],
    )
}
#[test]
fn audit_rejects_tampered_raw_artifacts_orphans_null_witness_and_runtime_profile_changes() {
    for kind in [
        "console",
        "mgmt",
        "witness",
        "null",
        "orphan",
        "classification",
        "manifest",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let batch = copy_batch(directory.path());
        assert!(verify_batch(&batch).status.success());
        match kind {
            "console" | "mgmt" => {
                let path = batch.join(format!("s1009-r0.{kind}"));
                let mut bytes = fs::read(&path).unwrap();
                bytes.extend(b"TAMPERED\n");
                fs::write(path, bytes).unwrap();
            }
            "witness" => update(&batch.join("s1009-r0.witness.json"), |v| {
                v["sha256"] = "0".repeat(64).into()
            }),
            "orphan" => fs::write(batch.join("s1009-r9.console"), "orphan").unwrap(),
            "manifest" => update(&batch.join("s1009-r0.manifest.json"), |v| {
                v["seed"] = 1010.into()
            }),
            "null" => {
                update(&batch.join("runs.json"), |v| v[0]["witness"] = Value::Null);
                fs::remove_file(batch.join("s1009-r0.witness.json")).unwrap();
            }
            _ => update(&batch.join("runs.json"), |v| {
                v[0]["reason"] = "pass fabricated".into()
            }),
        }
        assert!(!verify_batch(&batch).status.success(), "accepted {kind}");
    }
    for key in [
        "witness_plugin_sha256",
        "qemu_sha256",
        "kernel_sha256",
        "shift",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let batch = copy_batch(directory.path());
        let manifest = batch.join("s1009-r0.manifest.json");
        let reference = directory.path().join("reference");
        fs::copy(&manifest, &reference).unwrap();
        let verify = || {
            python_api("tools/mac-tcg/service_audit.py","subject.verify_profiles([Path(sys.argv[2])],json.loads(Path(sys.argv[3]).read_text()))",&[&batch,&reference])
        };
        assert!(verify().status.success());
        update(&manifest, |v| v[key] = "mismatched".into());
        assert!(!verify().status.success());
    }
    let directory = tempfile::tempdir().unwrap();
    let batch = directory.path().join("service-interrupted");
    fs::create_dir(&batch).unwrap();
    fs::write(batch.join("s1-r0.console"), "partial").unwrap();
    let out=python_api("tools/mac-tcg/service_audit.py","subject.verify_inventory(Path(sys.argv[2]),{'limit_seconds':600,'spent_seconds':0,'runs':[]})",&[directory.path()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unreported attempt"));
}

#[test]
fn failed_atomic_save_keeps_the_previous_document() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("budget.json");
    assert!(python_api(
        "tools/mac-tcg/run.py",
        "subject.save(Path(sys.argv[2]),{'old':1})",
        &[&path]
    )
    .status
    .success());
    assert!(!python_api(
        "tools/mac-tcg/run.py",
        "subject.save(Path(sys.argv[2]),{'new':set()})",
        &[&path]
    )
    .status
    .success());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
        json!({"old":1})
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn rejected_raw_witnesses_remain_auditable_and_cannot_be_changed_after_classification() {
    for raw in [
        b"{malformed".as_slice(),
        b"{\"schema\":\"seedbox.tcg.boundaries.v2\",\"terminal\":false}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let batch = copy_batch(directory.path());
        let path = batch.join("s1009-r0.witness.json");
        fs::write(&path, raw).unwrap();
        let decoded = result(&python_api(
            "tools/mac-tcg/service.py",
            "subject.read_witness(Path(sys.argv[2]),0)",
            &[&path],
        ));
        assert!(decoded[0].is_null());
        assert!(!decoded[2].is_null());
        update(&batch.join("runs.json"), |rows| {
            rows[0]["status"] = "ERROR".into();
            rows[0]["reason"] = "missing or malformed requested witness".into();
            rows[0]["witness"] = decoded[0].clone();
            rows[0]["witness_diagnostics"] = decoded[1].clone();
            rows[0]["witness_rejected_sha256"] = decoded[2].clone();
        });
        let rows: Value =
            serde_json::from_slice(&fs::read(batch.join("runs.json")).unwrap()).unwrap();
        let mut row = rows[0].clone();
        row["path"] = batch.join("s1009-r0").to_string_lossy().into_owned().into();
        let budget = directory.path().join("budget.json");
        fs::write(
            &budget,
            json!({"limit_seconds":600,"spent_seconds":row["wall_seconds"],"runs":[row]})
                .to_string(),
        )
        .unwrap();
        let verify = || {
            python_api("tools/mac-tcg/service_audit.py","subject.verify_inventory(Path(sys.argv[2]),json.loads(Path(sys.argv[3]).read_text()))",&[directory.path(),&budget])
        };
        assert!(verify().status.success());
        let mut tampered = raw.to_vec();
        tampered.extend(b"TAMPERED");
        fs::write(path, tampered).unwrap();
        assert!(!verify().status.success());
    }
}

#[test]
fn terminal_decoder_uses_complete_opcodes_instead_of_suffix_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("probe.c");
    let binary = directory.path().join("probe");
    fs::write(&source,"#include \"witness-decode.h\"\n#include <stdio.h>\n#include <stdlib.h>\nint main(int argc,char **argv) { unsigned char bytes[16]; for(int i=1;i<argc;i++) bytes[i-1]=strtoul(argv[i],NULL,16); printf(\"%d\\n\",terminal_out(bytes,argc-1)); }\n").unwrap();
    support::success(
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-Wall", "-Wextra", "-Werror", "-I"])
            .arg(support::root().join("tools/mac-tcg"))
            .arg(source)
            .arg("-o")
            .arg(&binary),
    );
    for (expected, bytes) in [
        (0, vec![0x3c, 0xee]),
        (0, vec![0xe6, 0xee]),
        (0, vec![0xe7, 0xef]),
        (0, vec![0xcd, 0xee]),
        (0, vec![0xb8, 0, 0, 0xe7, 0xf4]),
        (1, vec![0xee]),
        (1, vec![0x66, 0xef]),
        (1, vec![0xf3, 0xee]),
        (2, vec![0xe6, 0xf4]),
        (2, vec![0x66, 0xe7, 0xf4]),
        (2, vec![0x48, 0xe6, 0xf4]),
        (0, vec![0x66]),
        (0, vec![0xe6]),
        (0, vec![0xee, 0]),
    ] {
        let output = support::success(
            Command::new(&binary).args(bytes.iter().map(|byte| format!("{byte:x}"))),
        );
        assert_eq!(output.stdout, format!("{expected}\n").as_bytes());
    }
}
