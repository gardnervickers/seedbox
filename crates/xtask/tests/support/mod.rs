#![allow(dead_code)]
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

pub fn success(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Foreign-function adapter only: all fixtures, expectations and assertions
/// belong to the calling Rust integration test. Each call is its own process.
pub fn python_api(module: &str, expression: &str, args: &[&Path]) -> Output {
    let script = format!(
        r#"
import importlib.util, json, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location('subject', sys.argv[1])
sys.path.insert(0, str(Path(sys.argv[1]).parent))
subject = importlib.util.module_from_spec(spec)
sys.modules['subject'] = subject
spec.loader.exec_module(subject)
value = {expression}
print(json.dumps(value))
"#
    );
    Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root().join(module))
        .args(args)
        .output()
        .unwrap()
}

pub fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
