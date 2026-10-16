//! Rust-owned cases for production Python kernel build tools (no ARM build/guest).
//! Python only invokes functions without CLI boundaries and marshals their values.
//! apply/audit/check-config/build-initramfs.py remain production build tools:
//! they patch Linux, audit compiled instructions, validate Kconfig and emit newc.
//! These tests exercise those implementations, not Rust copies of their policy.
mod support;

use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, process::Command};
use support::{diagnostic, root, run};

const BRIDGE: &str = r#"import json, pathlib, runpy, sys
m = runpy.run_path(sys.argv[1])
try:
    result = eval(sys.argv[2])
except ValueError as error:
    print(json.dumps({'exception': 'ValueError', 'message': str(error)}))
else:
    print(json.dumps({'result': result}))
"#;

fn invoke(tool: &str, expression: &str, arguments: &[&str]) -> Result<Value, String> {
    let output = run(
        Command::new("python3")
            .args(["-c", BRIDGE])
            .arg(root().join("kernel-arm64").join(tool))
            .arg(expression)
            .args(arguments),
        10,
    );
    if !output.status.success() {
        return Err(diagnostic(&output));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("{error}: {}", diagnostic(&output)))
}

fn value(tool: &str, expression: &str, arguments: &[&str]) -> Value {
    let envelope = invoke(tool, expression, arguments).unwrap();
    assert!(envelope.get("exception").is_none(), "{envelope}");
    envelope.get("result").expect("result envelope").clone()
}

fn value_error(tool: &str, expression: &str, arguments: &[&str]) {
    let envelope = invoke(tool, expression, arguments).expect("interpreter must complete normally");
    assert_eq!(envelope["exception"], "ValueError", "{envelope}");
    assert!(envelope.get("result").is_none(), "{envelope}");
}

#[test]
fn fixture_requires_hrtimers_and_timerfd_even_if_fragment_omits_them() {
    let required = json!({"CONFIG_HIGH_RES_TIMERS": "y", "CONFIG_TIMERFD": "y"});
    let expression = "m['fixture_errors'](json.loads(sys.argv[3]))";
    assert_eq!(
        value("check-config.py", expression, &[&required.to_string()]),
        json!([])
    );
    for key in ["CONFIG_HIGH_RES_TIMERS", "CONFIG_TIMERFD"] {
        for setting in [None, Some("n"), Some("m")] {
            let mut actual = required.clone();
            match setting {
                None => {
                    actual.as_object_mut().unwrap().remove(key);
                }
                Some(setting) => {
                    actual[key] = json!(setting);
                }
            }
            assert_eq!(
                value("check-config.py", expression, &[&actual.to_string()]),
                json!([format!("{key}: timerfd spin fixture requires y")])
            );
        }
    }
    assert_eq!(
        value("check-config.py", expression, &["{}"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn config_rejects_unknown_disabled_and_dropped_requests() {
    // Keep settings parsing and requested insertion order in the production functions.
    let expression = "m['errors'](m['settings'](sys.argv[3]), m['settings'](sys.argv[4]), set(json.loads(sys.argv[5])))";
    assert_eq!(
        value(
            "check-config.py",
            expression,
            &[
                "CONFIG_PRESENT=y\nCONFIG_REMOVED=n\nCONFIG_HZ=100\n",
                "# CONFIG_PRESENT is not set\nCONFIG_HZ=1000\n",
                r#"["CONFIG_PRESENT", "CONFIG_HZ"]"#,
            ]
        ),
        json!([
            "CONFIG_PRESENT: requested y, resolved n",
            "CONFIG_REMOVED: not defined in arm64 Kconfig sources",
            "CONFIG_HZ: requested 100, resolved 1000",
        ])
    );
    assert_eq!(
        value(
            "check-config.py",
            expression,
            &["# CONFIG_PRESENT is not set\n", "", r#"["CONFIG_PRESENT"]"#,]
        ),
        json!([])
    );
    value_error(
        "check-config.py",
        "m['settings'](sys.argv[3])",
        &["CONFIG_PRESENT=y\nCONFIG_PRESENT=n\n"],
    );
}

#[test]
fn config_definitions_exclude_other_architectures() {
    let directory = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("arch/Kconfig", "config GENERIC\n"),
        ("arch/arm64/Kconfig", "\tconfig ARM64_ONLY\n"),
        ("arch/x86/Kconfig", "config X86_ONLY\n"),
        ("drivers/Kconfig", "menuconfig DRIVER\n"),
    ] {
        let path = directory.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let result = value(
        "check-config.py",
        "list(m['defined_symbols'](pathlib.Path(sys.argv[3])))",
        &[directory.path().to_str().unwrap()],
    );
    let mut symbols: Vec<_> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    symbols.sort();
    assert_eq!(
        symbols,
        ["CONFIG_ARM64_ONLY", "CONFIG_DRIVER", "CONFIG_GENERIC"]
    );
}

#[derive(Debug, PartialEq)]
struct Entry {
    mode: u32,
    data: Vec<u8>,
    device: [u32; 2],
}

// Independent binary newc reader, not a call back into the archive writer.
fn parse_newc(image: &[u8]) -> BTreeMap<String, Entry> {
    let mut entries = BTreeMap::new();
    let mut offset = 0;
    loop {
        assert_eq!(&image[offset..offset + 6], b"070701");
        let mut fields = [0; 13];
        for (i, field) in fields.iter_mut().enumerate() {
            *field = u32::from_str_radix(
                std::str::from_utf8(&image[offset + 6 + i * 8..offset + 14 + i * 8]).unwrap(),
                16,
            )
            .unwrap();
        }
        let [mode, size, namesize] = [fields[1], fields[6], fields[11]];
        offset += 110;
        let name = std::str::from_utf8(&image[offset..offset + namesize as usize - 1])
            .unwrap()
            .to_owned();
        assert_eq!(image[offset + namesize as usize - 1], 0);
        offset = (offset + namesize as usize + 3) & !3;
        let data = image[offset..offset + size as usize].to_vec();
        offset = (offset + size as usize + 3) & !3;
        entries.insert(
            name.clone(),
            Entry {
                mode,
                data,
                device: [fields[9], fields[10]],
            },
        );
        if name == "TRAILER!!!" {
            break;
        }
    }
    entries
}

#[test]
fn newc_independent_parse() {
    let archive = || {
        // Hex is a transport encoding only; Rust owns the byte parser and expectations.
        let hex = value(
            "build-initramfs.py",
            "m['archive'](sys.argv[3].encode()).hex()",
            &["ELF-test"],
        );
        let (pairs, remainder) = hex.as_str().unwrap().as_bytes().as_chunks::<2>();
        assert!(remainder.is_empty(), "odd-length archive hex transport");
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
    };
    let image = archive();
    assert_eq!(image, archive());
    assert_eq!(image.len() % 512, 0);
    let entries = parse_newc(&image);
    assert_eq!(
        entries["init"],
        Entry {
            mode: 0o100755,
            data: b"ELF-test".to_vec(),
            device: [0, 0]
        }
    );
    assert_eq!(
        entries["dev/console"],
        Entry {
            mode: 0o020600,
            data: vec![],
            device: [5, 1]
        }
    );
}

#[test]
fn patch_refuses_drift() {
    assert_eq!(
        value(
            "apply.py",
            "m['body'](*json.loads(sys.argv[3]))",
            &[&json!([
                "int f(void) { if (a) { b(); } return 0; }",
                "f",
                "return 1;"
            ])
            .to_string()]
        ),
        "int f(void) {\nreturn 1;\n}"
    );
    value_error(
        "apply.py",
        "m['body'](*json.loads(sys.argv[3]))",
        &[&json!(["int f(void);", "f", ""]).to_string()],
    );
    value_error(
        "apply.py",
        "m['replace'](*json.loads(sys.argv[3]))",
        &[&json!(["xx", "x", "y"]).to_string()],
    );
}

#[test]
fn exec_cannot_reenable_hardware_counter() {
    let source = include_str!("fixtures/arm-kernel/counter-access.c");
    let patched = value("apply.py", "m['counter_access'](sys.argv[3])", &[source]);
    let patched = patched.as_str().unwrap();
    assert!(!patched.contains("cntkctl_el1, 0, ARCH_TIMER_USR_VCT_ACCESS_EN"));
    assert_eq!(
        patched
            .matches("cntkctl_el1, ARCH_TIMER_USR_VCT_ACCESS_EN, 0")
            .count(),
        2
    );
    assert!(patched.contains("TIF_TSC_SIGSEGV"));
    value_error("apply.py", "m['counter_access'](sys.argv[3])", &[patched]);
}

#[test]
fn counter_audit_rejects_real_and_encoded_leaks() {
    for instruction in [
        "mrs x0, cntvct_el0",
        "mrs x4, cntpctss_el0",
        "mrs x0, s3_3_c14_c0_6",
        "mrs x0, RNDR",
        "wfit x3",
        "mrs x0, CNTFRQ_EL0",
    ] {
        let result = value("audit.py", "m['violations'](sys.argv[3])", &[instruction]);
        assert!(!result.as_array().unwrap().is_empty(), "{instruction}");
    }
    assert_eq!(
        value(
            "audit.py",
            "m['violations'](sys.argv[3])",
            &["msr cntv_ctl_el0, xzr\nmrs x0, cntkctl_el1\n<arch_timer_read_cntvct_el0>"]
        ),
        json!([])
    );
}

#[test]
fn interpreter_failure_is_not_a_production_value_error() {
    // A real uncaught interpreter exception must never satisfy assertRaises(ValueError).
    let error = invoke("apply.py", "missing_interpreter_name", &[]).unwrap_err();
    assert!(error.contains("NameError"), "{error}");
    assert!(
        std::panic::catch_unwind(|| {
            value_error("apply.py", "missing_interpreter_name", &[]);
        })
        .is_err(),
        "the expected-ValueError assertion must reject interpreter failure"
    );
    value_error(
        "apply.py",
        "m['replace'](*json.loads(sys.argv[3]))",
        &[&json!(["xx", "x", "y"]).to_string()],
    );
}
