//! Host-side tools: building the guest oracle binary, and cross-checking the
//! list-append checker against Jepsen's Elle. Both drive the user's
//! Docker-compatible engine through the `docker` CLI (`DOCKER_HOST`).
use crate::checker::{edn, elle};
use crate::kit;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The repository root this binary was built from.
pub fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(command: &mut Command) -> Result<Vec<u8>, String> {
    let shown = format!("{command:?}");
    let output = command
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("{shown}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{shown}: {}", output.status));
    }
    Ok(output.stdout)
}

fn docker(engine: Option<&str>) -> Command {
    let mut command = Command::new("docker");
    if let Some(engine) = engine {
        command.env("DOCKER_HOST", engine);
    }
    command
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Build the static Linux oracle binary for `arch` inside a Rust container
/// and write it to `build/jepsen/<arch>/seedbox-jepsen`. The build context
/// holds only this crate, its in-repository path dependencies and the
/// vendored registry and git sources (the host fetches them, so the private
/// `wgl` repository needs no credentials in the container).
pub fn build_oracle(root: &Path, arch: &str, engine: Option<&str>) -> Result<PathBuf, String> {
    let root = fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let crate_dir = root.join("crates/seedbox-jepsen");
    let context = crate_dir.join("target/oracle-context");
    if context.exists() {
        fs::remove_dir_all(&context).map_err(|e| format!("{}: {e}", context.display()))?;
    }
    fs::create_dir_all(context.join(".cargo")).map_err(|e| e.to_string())?;
    let config = run(Command::new("cargo")
        .current_dir(&root)
        .args(["vendor", "--locked", "--versioned-dirs", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml"))
        .arg(context.join("vendor")))?;
    let config = String::from_utf8_lossy(&config)
        .replace(&context.join("vendor").display().to_string(), "vendor");
    fs::write(context.join(".cargo/config.toml"), config).map_err(|e| e.to_string())?;
    for (from, to) in [
        ("crates/seedbox-jepsen/src", "crates/seedbox-jepsen/src"),
        (
            "crates/seedbox-control-client/src",
            "crates/seedbox-control-client/src",
        ),
        ("crates/seedbox-control/src", "crates/seedbox-control/src"),
    ] {
        copy_tree(&root.join(from), &context.join(to))?;
    }
    for file in [
        "crates/seedbox-jepsen/Cargo.toml",
        "crates/seedbox-jepsen/Cargo.lock",
        "crates/seedbox-control-client/Cargo.toml",
        "crates/seedbox-control/Cargo.toml",
        "crates/seedbox-engine-core/build.rs",
        "crates/seedbox-jepsen/oracle.Dockerfile",
    ] {
        let target = context.join(file);
        fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::copy(root.join(file), &target).map_err(|e| format!("{file}: {e}"))?;
    }
    // The path dependencies inherit package keys from the Seedbox workspace;
    // keep its manifest but only the members present here.
    let manifest = fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?;
    let manifest: String = manifest
        .lines()
        .filter(|line| !line.starts_with("default-members"))
        .map(|line| {
            if line.starts_with("members") {
                "members = [\"crates/seedbox-control\", \"crates/seedbox-control-client\"]\n"
                    .to_string()
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    fs::write(context.join("Cargo.toml"), manifest).map_err(|e| e.to_string())?;
    let platform = match arch {
        "arm64" => "linux/arm64",
        "amd64" => "linux/amd64",
        other => return Err(format!("--arch {other}: expected arm64 or amd64")),
    };
    let output = root.join(format!("build/jepsen/{arch}"));
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    run(docker(engine)
        .args(["build", "--quiet", "--platform", platform, "-f"])
        .arg(context.join("crates/seedbox-jepsen/oracle.Dockerfile"))
        .arg("--output")
        .arg(format!("type=local,dest={}", output.display()))
        .arg(&context))?;
    let output = output.join("seedbox-jepsen");
    if !output.is_file() {
        return Err(format!(
            "{}: the build exported no binary",
            output.display()
        ));
    }
    Ok(output)
}

// ------------------------------------------------------------ Elle cross-check

const ELLE_ZIP_URL: &str =
    "https://github.com/ligurio/elle-cli/releases/download/0.1.11/elle-cli-bin-0.1.11.zip";
const ELLE_ZIP_SHA256: &str = "9d3c72eaf3ccc4d0c7dc1da710264dee01f03af58c9fa177fec3f78ff5af1779";
const ELLE_JAR_SHA256: &str = "85c39a6247509075bf21762d4c8ed3a2cb3d2c9b2c169cb2c80dde25fa760388";
const ELLE_JAR: &str = "elle-cli-0.1.11-standalone.jar";
/// elle-cli 0.1.11 needs Java 21 with the jdk.random module, which the JRE image lacks.
const ELLE_JDK: &str =
    "docker.io/library/eclipse-temurin:21-jdk@sha256:f9bf7e9fe2f9bc4657537c1a7e20c768f0c3bafca41598263be4714783dd62eb";
/// Elle's names for our cycle types.
const ELLE_NAMES: [(&str, &str); 2] = [
    ("G-single-item", "G-single"),
    ("G-nonadjacent-item", "G2-item"),
];

fn sha256(path: &Path) -> Result<String, String> {
    let output = run(Command::new("shasum").args(["-a", "256"]).arg(path))?;
    Ok(String::from_utf8_lossy(&output)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string())
}

/// Fetch the pinned elle-cli jar once and verify it.
fn elle_jar(cache: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    let jar = cache.join(ELLE_JAR);
    if !jar.exists() {
        let zip = cache.join("elle.zip");
        run(Command::new("curl")
            .args(["-sSL", "-o"])
            .arg(&zip)
            .arg(ELLE_ZIP_URL))?;
        if sha256(&zip)? != ELLE_ZIP_SHA256 {
            return Err(format!("{}: unexpected SHA-256", zip.display()));
        }
        let bytes = run(Command::new("unzip")
            .arg("-p")
            .arg(&zip)
            .arg(format!("target/{ELLE_JAR}")))?;
        fs::write(&jar, bytes).map_err(|e| e.to_string())?;
    }
    if sha256(&jar)? != ELLE_JAR_SHA256 {
        return Err(format!("{}: unexpected SHA-256", jar.display()));
    }
    Ok(jar)
}

/// One history to cross-check: a label, an oracle log of `H` lines, and its models.
#[derive(Clone, Debug)]
pub struct CrossCheck {
    pub label: String,
    pub log: PathBuf,
    pub models: Vec<String>,
}

/// Parse `LABEL=LOG[@model,model]` (a model list after the label also works).
pub fn parse_history(
    item: &str,
    default_models: &[String],
    base: &Path,
) -> Result<CrossCheck, String> {
    let (label, log) = item
        .split_once('=')
        .ok_or_else(|| format!("{item}: expected LABEL=LOG"))?;
    let (label, models) = match label.split_once('@') {
        Some((label, models)) => (label, models.split(',').map(str::to_owned).collect()),
        None => (label, default_models.to_vec()),
    };
    let log = Path::new(log);
    Ok(CrossCheck {
        label: label.into(),
        log: if log.is_absolute() {
            log.into()
        } else {
            base.join(log)
        },
        models,
    })
}

fn summarize_elle(result: &Value) -> Value {
    if let Some(error) = result.get("error") {
        return json!({"valid": "error", "counts": {}, "error": error});
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (kind, items) in result["anomalies"].as_object().into_iter().flatten() {
        let mapped = ELLE_NAMES
            .iter()
            .find(|(e, _)| e == kind)
            .map(|(_, o)| *o)
            .unwrap_or(kind);
        *counts.entry(mapped.into()).or_default() += items.as_array().map(Vec::len).unwrap_or(1);
    }
    json!({"valid": result["valid?"], "counts": counts, "raw": result["anomaly-types"]})
}

/// Run elle-cli on every history for every model in a JDK container, then
/// compare its validity, anomaly types and counts with ours. Elle searches
/// only for the anomalies a model forbids, so only those are compared.
/// Returns the comparison rows and whether every verdict agreed.
pub fn elle_cross_check(
    histories: &[CrossCheck],
    elle_only: &[String],
    engine: Option<&str>,
    cpus: &str,
    work: &Path,
) -> Result<(Vec<Value>, bool), String> {
    let jar = elle_jar(&work.join("jar"))?;
    let staging = work.join("h");
    let out = work.join("out");
    for dir in [&staging, &out] {
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut models: Vec<String> = histories
        .iter()
        .flat_map(|h| h.models.clone())
        .chain(elle_only.iter().cloned())
        .collect();
    models.sort();
    models.dedup();
    let mut loaded = BTreeMap::new();
    for history in histories {
        let text = fs::read_to_string(&history.log)
            .map_err(|e| format!("{}: {e}", history.log.display()))?;
        let txns = elle::from_h_lines(&kit::tagged_lines(&text, "H "))?;
        let lines = edn::lines(&txns);
        fs::write(
            staging.join(format!("{}.edn", history.label)),
            lines.join("\n") + "\n",
        )
        .map_err(|e| e.to_string())?;
        loaded.insert(history.label.clone(), txns);
    }
    fs::write(staging.join("models.txt"), models.join("\n") + "\n").map_err(|e| e.to_string())?;
    // The jar travels with the histories, so no image needs building or loading.
    fs::copy(&jar, staging.join("elle.jar")).map_err(|e| e.to_string())?;
    let script = "set -eu; mkdir -p /h /out; tar -x -C /h; \
        for model in $(cat /h/models.txt); do for f in /h/*.edn; do b=$(basename $f .edn); \
        java -Xmx3g -jar /h/elle.jar -m list-append -c $model -v json $f > /out/$b.$model.json 2> /out/$b.$model.err || true; \
        done; done; tar -c -C /out .";
    let archive = run(Command::new("tar")
        .arg("-C")
        .arg(&staging)
        .args(["-cf", "-", "."]))?;
    let mut child = docker(engine)
        .args([
            "run",
            "--rm",
            "-i",
            "--cpuset-cpus",
            cpus,
            ELLE_JDK,
            "sh",
            "-c",
            script,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("docker run: {e}"))?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let writer = std::thread::spawn(move || stdin.write_all(&archive));
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        let _ = writer.join();
        if !output.status.success() {
            return Err(format!("elle container: {}", output.status));
        }
        let mut untar = Command::new("tar")
            .arg("-C")
            .arg(&out)
            .args(["-xf", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        untar
            .stdin
            .take()
            .expect("piped")
            .write_all(&output.stdout)
            .map_err(|e| e.to_string())?;
        untar.wait().map_err(|e| e.to_string())?;
    }
    let elle_result = |label: &str, model: &str| -> Value {
        let path = out.join(format!("{label}.{model}.json"));
        let text = fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_else(|_| {
            let error =
                fs::read_to_string(out.join(format!("{label}.{model}.err"))).unwrap_or_default();
            let tail: String = error
                .chars()
                .rev()
                .take(2000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            json!({"error": if tail.is_empty() { text } else { tail }})
        })
    };
    let mut rows = Vec::new();
    let mut agree_all = true;
    for history in histories {
        let txns = &loaded[&history.label];
        for model in &history.models {
            let ours = elle::check(txns, model)?;
            let theirs = summarize_elle(&elle_result(&history.label, model));
            let forbidden: BTreeMap<&String, &usize> = ours
                .counts
                .iter()
                .filter(|(k, _)| ours.illegal.contains(k))
                .collect();
            let allowed: BTreeMap<&String, &usize> = ours
                .counts
                .iter()
                .filter(|(k, _)| !ours.illegal.contains(k))
                .collect();
            let theirs_counts: BTreeMap<String, usize> =
                serde_json::from_value(theirs["counts"].clone()).unwrap_or_default();
            let agree_valid = theirs["valid"] == ours.valid;
            let agree_types = forbidden
                .keys()
                .map(|k| k.as_str())
                .eq(theirs_counts.keys().map(String::as_str));
            let agree_counts = forbidden
                .iter()
                .map(|(k, v)| (k.as_str(), **v))
                .eq(theirs_counts.iter().map(|(k, v)| (k.as_str(), *v)));
            agree_all &= agree_valid;
            rows.push(
                json!({"history": history.label, "model": model, "txns": ours.txns,
                             "ours": {"valid": ours.valid, "counts": forbidden, "allowed": allowed},
                             "elle": theirs, "agree_valid": agree_valid, "agree_types": agree_types,
                             "agree_counts": agree_counts}),
            );
        }
        for model in elle_only {
            rows.push(
                json!({"history": history.label, "model": model, "ours": null,
                             "elle": summarize_elle(&elle_result(&history.label, model))}),
            );
        }
    }
    Ok((rows, agree_all))
}

/// The comparison as a Markdown table.
pub fn cross_check_table(rows: &[Value]) -> String {
    let side = |side: &Value| -> String {
        if side.is_null() {
            return "(not checked)".into();
        }
        let counts = side["counts"].as_object().map(|c| {
            c.iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        });
        let counts = counts
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "none".into());
        let allowed = side["allowed"].as_object().map(|c| {
            c.iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        });
        let valid = match &side["valid"] {
            Value::Bool(true) => "True".to_string(),
            Value::Bool(false) => "False".to_string(),
            other => other.as_str().unwrap_or("?").to_string(),
        };
        match allowed.filter(|a| !a.is_empty()) {
            Some(allowed) => format!("{valid}: {counts} (allowed: {allowed})"),
            None => format!("{valid}: {counts}"),
        }
    };
    let mut lines = vec![
        "| History | Model | Txns ok/fail/info | Ours | Elle | Agree |".to_string(),
        "| --- | --- | --- | --- | --- | --- |".to_string(),
    ];
    for row in rows {
        let txns = &row["txns"];
        let txns = if txns.is_null() {
            String::new()
        } else {
            format!(
                "{}/{}/{}",
                txns["ok"].as_u64().unwrap_or(0),
                txns["fail"].as_u64().unwrap_or(0),
                txns["info"].as_u64().unwrap_or(0)
            )
        };
        let agree = if row["ours"].is_null() {
            "(Elle only)"
        } else if row["agree_counts"] == true {
            "yes"
        } else if row["agree_valid"] == true && row["agree_types"] == true {
            "verdict and types (counts differ)"
        } else if row["agree_valid"] == true {
            "verdict only"
        } else {
            "**NO**"
        };
        lines.push(format!(
            "| {} | {} | {txns} | {} | {} | {agree} |",
            row["history"].as_str().unwrap_or(""),
            row["model"].as_str().unwrap_or(""),
            side(&row["ours"]),
            side(&row["elle"])
        ));
    }
    lines.join("\n")
}
