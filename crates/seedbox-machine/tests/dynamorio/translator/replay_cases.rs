//! Finite public decoder/IR regressions. Cross-builds are not native ARM evidence.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn fixtures() -> PathBuf {
    PathBuf::from(std::env::var_os("SEEDBOX_DYNAMORIO_FIXTURES").expect("prepared DR fixtures"))
        .canonicalize()
        .expect("fixture root")
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

#[test]
fn shared_public_ir_transforms_and_negative_controls() {
    for suffix in ["", "_standalone"] {
        run(
            Command::new(fixtures().join(format!("bin/translator_replay_shared{suffix}"))),
            &format!("shared public IR controls{suffix} (supplemental A64 coverage)"),
            "shared-public-ir: PASS cases=256 properties=8 negative-controls=256",
        )
        .unwrap();
    }
}

#[test]
fn retained_replay_inputs_match_original_bytes() {
    let root = fixtures();
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
    let root = fixtures();
    let mut failures = Vec::new();
    for row in include_str!("replay/cases.tsv").lines().skip(1) {
        let fields: Vec<_> = row.split('\t').collect();
        for suffix in ["", "_standalone"] {
            let mut command =
                Command::new(root.join(format!("bin/translator_replay_{}{suffix}", fields[1])));
            command.arg(root.join("share/translator/saved").join(fields[2]));
            if let Err(error) = run(
                command,
                &format!("{}{suffix}", fields[0]),
                "saved-input: PASS",
            ) {
                failures.push(error);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a64_original_eight_ir_transform_populations() {
    let root = fixtures();
    let mut failures = Vec::new();
    for suffix in ["", "_standalone"] {
        let mut command =
            Command::new(root.join(format!("bin/translator_replay_ir_transform{suffix}")));
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
        let out = match run(
            command,
            &format!("DR-IR-transform{suffix}: all eight original properties"),
            "all properties hold",
        ) {
            Ok(out) => out,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
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
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
