//! Publication ownership checks against the actual Python guest-input packer.
//! guest-assets.py remains a production build tool assembling already-built inputs.
//! Rust owns fixtures/cases/assertions; Python only invokes main, injects boundary
//! faults and marshals actual exceptions. No guest or compiler is involved.
mod support;

use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::Command};
use support::{diagnostic, root, run};

const INPUTS: [&str; 4] = ["kernel", "init", "command-line", "firmware"];
const LOAD: &str = r#"import errno, json, os, pathlib, runpy, sys
packer = runpy.run_path(sys.argv[1])
paths = json.loads(sys.argv[3])
sys.argv = json.loads(sys.argv[2])
events = []
"#;
const CROSS_DEVICE: &str = r#"def rename(source, destination):
    events.append('rename')
    raise OSError(errno.EXDEV, 'different filesystem')
pathlib.Path.rename = rename
"#;
const CONCURRENT: &str = r#"def rename(source, destination):
    events.append('rename')
    os.rename(paths['concurrent'], destination)
    raise OSError(errno.EXDEV, 'different filesystem')
pathlib.Path.rename = rename
"#;
const COPY_FAILURE: &str = r#"def copy(source, destination, **kwargs):
    events.append('copy')
    os.rename(paths['partial'], destination / 'partial')
    raise OSError('copy failed')
packer['shutil'].copytree = copy
"#;
const INTERPRETER_FAILURE: &str = r#"def rename(source, destination):
    raise NameError('injected interpreter failure at rename')
pathlib.Path.rename = rename
"#;
const INVOKE: &str = r#"try:
    packer['main']()
except (SystemExit, OSError) as error:
    print(json.dumps({'exception': type(error).__name__, 'message': str(error), 'events': events}))
else:
    print(json.dumps({'result': None, 'events': events}))
"#;

#[derive(Clone, Copy)]
enum Fault {
    None,
    CrossDevice,
    Concurrent,
    Copy,
    Interpreter,
}

struct Fixture {
    directory: tempfile::TempDir,
    output: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        for name in INPUTS {
            fs::write(
                directory.path().join(name),
                if name == "command-line" {
                    b""
                } else {
                    name.as_bytes()
                },
            )
            .unwrap();
        }
        let output = directory.path().join("output");
        Self { directory, output }
    }

    fn invoke(&self, fault: Fault) -> Result<Value, String> {
        // These are the original argv, including a truly empty command-line file.
        let path = |name| {
            self.directory
                .path()
                .join(name)
                .to_str()
                .unwrap()
                .to_owned()
        };
        let argv = json!([
            "guest-assets.py",
            "tcg",
            "--kernel",
            path("kernel"),
            "--init",
            path("init"),
            "--command-line-file",
            path("command-line"),
            "--firmware",
            path("firmware"),
            "--output",
            self.output.to_str().unwrap(),
        ]);
        let paths = json!({"concurrent": path("concurrent"), "partial": path("partial-source")});
        let injection = match fault {
            Fault::None => String::new(),
            Fault::CrossDevice => CROSS_DEVICE.to_owned(),
            Fault::Concurrent => CONCURRENT.to_owned(),
            Fault::Copy => [CROSS_DEVICE, COPY_FAILURE].concat(),
            Fault::Interpreter => INTERPRETER_FAILURE.to_owned(),
        };
        let output = run(
            Command::new("python3")
                .args(["-c", &[LOAD, &injection, INVOKE].concat()])
                .arg(root().join("crates/seedbox-machine/guest-assets.py"))
                .arg(argv.to_string())
                .arg(paths.to_string()),
            10,
        );
        if !output.status.success() {
            return Err(diagnostic(&output));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("{error}: {}", diagnostic(&output)))
    }

    fn expect_exception(&self, fault: Fault, expected: &str, events: Value) {
        let actual = self
            .invoke(fault)
            .expect("interpreter must complete normally");
        assert_eq!(actual["exception"], expected, "{actual}");
        assert!(actual.get("result").is_none(), "{actual}");
        assert_eq!(actual["events"], events);
    }

    fn expect_success(&self, fault: Fault, events: Value) {
        let actual = self
            .invoke(fault)
            .expect("interpreter must complete normally");
        assert!(actual.get("exception").is_none(), "{actual}");
        assert_eq!(actual.get("result"), Some(&Value::Null), "{actual}");
        assert_eq!(actual["events"], events);
    }

    fn assert_exact_inputs(&self) {
        for name in INPUTS {
            assert_eq!(
                fs::read(self.output.join(name)).unwrap(),
                fs::read(self.directory.path().join(name)).unwrap(),
                "{name}"
            );
        }
    }
}

#[test]
fn existing_destination_is_preserved() {
    let fixture = Fixture::new();
    fs::create_dir(&fixture.output).unwrap();
    fs::write(fixture.output.join("sentinel"), b"other owner").unwrap();
    fixture.expect_exception(Fault::None, "SystemExit", json!([]));
    assert_eq!(
        fs::read(fixture.output.join("sentinel")).unwrap(),
        b"other owner"
    );
}

#[test]
fn concurrent_destination_is_preserved_during_cross_filesystem_fallback() {
    let fixture = Fixture::new();
    // Prepare the other owner's complete directory in Rust; the callback moves
    // it into place at the production rename boundary, just before EXDEV.
    let concurrent = fixture.directory.path().join("concurrent");
    fs::create_dir(&concurrent).unwrap();
    fs::write(concurrent.join("sentinel"), b"concurrent owner").unwrap();
    fixture.expect_exception(Fault::Concurrent, "FileExistsError", json!(["rename"]));
    assert_eq!(
        fs::read(fixture.output.join("sentinel")).unwrap(),
        b"concurrent owner"
    );
}

#[test]
fn failed_copy_removes_only_its_owned_output() {
    let fixture = Fixture::new();
    // The callback places this actual partial file inside the production-owned
    // output at copytree entry, then raises the original OSError.
    fs::write(fixture.directory.path().join("partial-source"), b"partial").unwrap();
    fixture.expect_exception(Fault::Copy, "OSError", json!(["rename", "copy"]));
    assert!(!fixture.output.exists());
    assert_eq!(
        fs::read(fixture.directory.path().join("kernel")).unwrap(),
        b"kernel"
    );
}

#[test]
fn cross_filesystem_copy_keeps_exact_inputs_and_empty_arguments() {
    let fixture = Fixture::new();
    fixture.expect_success(Fault::CrossDevice, json!(["rename"]));
    fixture.assert_exact_inputs();
}

#[test]
fn ordinary_publication_keeps_exact_inputs_and_empty_arguments() {
    let fixture = Fixture::new();
    fixture.expect_success(Fault::None, json!([]));
    fixture.assert_exact_inputs();
}

#[test]
fn interpreter_failure_is_not_an_expected_packer_exception() {
    let fixture = Fixture::new();
    let error = fixture.invoke(Fault::Interpreter).unwrap_err();
    assert!(error.contains("NameError"), "{error}");
    assert!(
        std::panic::catch_unwind(|| {
            fixture.expect_exception(Fault::Interpreter, "OSError", json!([]));
        })
        .is_err(),
        "expected-packer-exception assertion must reject interpreter failure"
    );
    assert!(!fixture.output.exists());
}
