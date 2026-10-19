//! Finite public decoder/IR regressions. Cross-builds are not native ARM evidence.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const RELEASE_FIXTURES: &str = "SEEDBOX_DYNAMORIO_FIXTURES";
const DEBUG_FIXTURES: &str = "SEEDBOX_DYNAMORIO_DEBUG_FIXTURES";

fn fixtures(variable: &str) -> PathBuf {
    let path = std::env::var_os(variable)
        .unwrap_or_else(|| panic!("set {variable} to the matching prepared DR fixture root"));
    PathBuf::from(path)
        .canonicalize()
        .unwrap_or_else(|error| panic!("{variable}: prepared fixture root: {error}"))
}

struct OwnedProcess(std::process::Child);
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            // This unreaped child still owns its process-group identity.
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        }
        let _ = self.0.wait();
    }
}

fn run(mut command: Command, label: &str, expected: &str) -> Result<String, String> {
    let temp = tempfile::tempdir().unwrap();
    let stdout = temp.path().join("stdout");
    let stderr = temp.path().join("stderr");
    command
        .env_clear()
        .env("HOME", temp.path())
        .current_dir(temp.path())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = OwnedProcess(
        command
            .guarded_spawn()
            .map_err(|e| format!("{label}: {e}"))?,
    );
    let guard = HangGuard::new(
        "finite public IR regression completion",
        Duration::from_secs(300),
    );
    let status = guard.wait_for(|| child.0.try_wait().transpose());
    drop(child);
    let out = fs::read_to_string(stdout).unwrap();
    let err = fs::read_to_string(stderr).unwrap();
    match status {
        Ok(Ok(status)) if status.success() && out.contains(expected) => {
            eprintln!("{label}\n{out}{err}");
            Ok(out)
        }
        result => Err(format!(
            "{label}: {result:?}; expected {expected}\n{out}{err}"
        )),
    }
}

fn shared_controls(variable: &str) {
    let root = fixtures(variable);
    for suffix in ["", "_standalone"] {
        run(
            Command::new(root.join(format!("bin/translator_replay_shared{suffix}"))),
            &format!("{variable}: shared public IR controls{suffix} (supplemental A64 coverage)"),
            "shared-public-ir: PASS cases=256 properties=8 negative-controls=256",
        )
        .unwrap();
    }
}

#[test]
fn shared_public_ir_transforms_and_negative_controls() {
    shared_controls(RELEASE_FIXTURES);
}

#[test]
fn debug_shared_public_ir_transforms_and_negative_controls() {
    shared_controls(DEBUG_FIXTURES);
}

#[test]
fn retained_replay_inputs_match_original_bytes() {
    let root = fixtures(RELEASE_FIXTURES);
    let rows: Vec<_> = include_str!("replay/cases.tsv").lines().skip(1).collect();
    assert_eq!(rows.len(), 50);
    for row in rows {
        let fields: Vec<_> = row.split('\t').collect();
        let bytes = fs::read(root.join("share/translator/saved").join(fields[2])).unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(bytes)),
            fields[3],
            "{}",
            fields[0]
        );
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_fifty_saved_inputs_public_replay() {
    let root = fixtures(RELEASE_FIXTURES);
    let mut failures = Vec::new();
    for row in include_str!("replay/cases.tsv").lines().skip(1) {
        let fields: Vec<_> = row.split('\t').collect();
        let mut command = Command::new(root.join(format!("bin/translator_replay_{}", fields[1])));
        command.arg(root.join("share/translator/saved").join(fields[2]));
        if let Err(error) = run(command, fields[0], "saved-input: PASS") {
            failures.push(error);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_original_eight_ir_transform_populations() {
    let root = fixtures(RELEASE_FIXTURES);
    let mut command = Command::new(root.join("bin/translator_replay_ir_transform"));
    // The original transform used drdecode's host-independent encoding eligibility.
    // Original fixed population and all original floors remain enabled.
    command.args([
        "-seed", "1", "-n", "400000", "-created", "40000", "-ilists", "8000",
    ]);
    let mut vectors: Vec<_> = fs::read_dir(root.join("share/translator/vectors"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    vectors.sort();
    command.args(vectors);
    let out = run(
        command,
        "DR-IR-transform: all eight original properties",
        "all properties hold",
    )
    .unwrap();
    for property in [
        "canon", "clone", "rawbits", "reloc", "rename", "regdeps", "ilist", "create",
    ] {
        assert!(
            out.lines()
                .any(|line| line.split_whitespace().next() == Some(property)),
            "missing property {property}: {out}"
        );
    }
}

#[cfg(target_arch = "aarch64")]
fn a64_selector_controls(variable: &str) {
    let root = fixtures(variable);
    for suffix in ["", "_standalone"] {
        run(
            Command::new(root.join(format!("bin/translator_replay_selectors{suffix}"))),
            &format!("{variable}: original A64 selector consistency{suffix}"),
            "source-selectors: PASS registers=410 restored=5 aliases=36 opcodes=4",
        )
        .unwrap();
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_original_selector_forward_reverse_and_aliases() {
    a64_selector_controls(RELEASE_FIXTURES);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn debug_a64_original_selector_forward_reverse_and_aliases() {
    a64_selector_controls(DEBUG_FIXTURES);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_public_isa_feature_queries_near_and_distant_pc_relative_operands() {
    for variable in [RELEASE_FIXTURES, DEBUG_FIXTURES] {
        let root = fixtures(variable);
        let out = run(
            Command::new(root.join("bin/translator_replay_feature_queries")),
            &format!("{variable}: six public A64 ISA-feature queries"),
            "B.cond near BASE",
        )
        .unwrap();
        assert_eq!(out, "B.cond near BASE\nADR near BASE\nADRP near BASE\nB.cond far BASE\nADR far BASE\nADRP far BASE\n");
    }
}
