//! Native seedbox.yaml parsing without builds, registry access, or execution.

mod native_project;

use crate::{invalid, Error, Result};
use serde_yaml_ng::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub use seedbox_artifacts::config::*;

/// Load explicit native project inputs; no process environment is read.
pub fn load(path: &Path, environment: &BTreeMap<String, String>) -> Result<Simulation> {
    let source = path.canonicalize()?;
    parse(&std::fs::read_to_string(&source)?, &source, environment)
}

pub fn parse(yaml: &str, source: &Path, host: &BTreeMap<String, String>) -> Result<Simulation> {
    parse_for_architecture(yaml, source, host, crate::guest::Architecture::Amd64)
}

/// The CLI selects a guest ISA explicitly. The library never derives it from
/// the host; historical callers retain the original amd64 default.
pub fn load_for_architecture(
    path: &Path,
    environment: &BTreeMap<String, String>,
    architecture: crate::guest::Architecture,
) -> Result<Simulation> {
    let source = std::fs::canonicalize(path)?;
    parse_for_architecture(
        &std::fs::read_to_string(&source)?,
        &source,
        environment,
        architecture,
    )
}

pub fn parse_for_architecture(
    yaml: &str,
    source: &Path,
    _host: &BTreeMap<String, String>,
    architecture: crate::guest::Architecture,
) -> Result<Simulation> {
    let doc: Value = serde_yaml_ng::from_str(yaml)?;
    reject_tags(&doc, "$")?;
    native_project::parse(doc, source, architecture)
}

fn validation(path: &str, message: &str) -> Error {
    Error::Validation {
        path: path.into(),
        message: message.into(),
    }
}

fn reject_tags(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Tagged(_) => return invalid(path, "YAML tags are unsupported"),
        Value::Mapping(map) => {
            for (key, value) in map {
                reject_tags(key, path)?;
                reject_tags(
                    value,
                    &format!("{path}.{}", key.as_str().unwrap_or("<key>")),
                )?;
            }
        }
        Value::Sequence(list) => {
            for (index, value) in list.iter().enumerate() {
                reject_tags(value, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn text(value: &Value, path: &str) -> Result<String> {
    let text = value
        .as_str()
        .ok_or_else(|| validation(path, "expected a string"))?
        .to_owned();
    if text.is_empty() {
        return invalid(path, "expected a nonempty string");
    }
    Ok(text)
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}
fn dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

pub(crate) fn guest_path(s: &str, path: &str) -> Result<()> {
    if !s.starts_with('/')
        || s.starts_with("//")
        || s.contains('\0')
        || s.split('/').any(|p| p == ".." || p == ".")
        || (s != "/" && (s.ends_with('/') || s.contains("//")))
    {
        return invalid(path, "expected a normalized absolute guest path");
    }
    Ok(())
}

fn local_path(value: &Value, path: &str, base: &Path, directory: bool) -> Result<PathBuf> {
    let raw = text(value, path)?;
    if raw.contains("://") || raw.starts_with("git@") || raw.starts_with('~') {
        return invalid(path, "only explicit local paths are supported");
    }
    let resolved = base
        .join(raw)
        .canonicalize()
        .map_err(|e| validation(path, &e.to_string()))?;
    if !(if directory {
        resolved.is_dir()
    } else {
        resolved.is_file()
    }) {
        return invalid(
            path,
            if directory {
                "expected a directory"
            } else {
                "expected a file"
            },
        );
    }
    Ok(resolved)
}

fn duration(value: &Value, path: &str, zero: bool) -> Result<u64> {
    let raw = text(value, path)?;
    let pos = raw
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| validation(path, "duration needs a unit: ns, us, ms, s, m, h"))?;
    let (number, unit) = raw.split_at(pos);
    let scale: u128 = match unit {
        "ns" => 1,
        "us" => 1000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        _ => return invalid(path, "unknown duration unit"),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || fraction.len() > 18
        || (number.contains('.') && fraction.is_empty())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return invalid(path, "invalid decimal duration");
    }
    let denominator = 10u128.pow(fraction.len() as u32);
    let fraction: u128 = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse()
            .map_err(|_| validation(path, "invalid duration"))?
    };
    let whole: u128 = whole
        .parse()
        .map_err(|_| validation(path, "invalid duration"))?;
    let fractional_ns = fraction * scale;
    if !fractional_ns.is_multiple_of(denominator) {
        return invalid(path, "duration must use whole nanoseconds");
    }
    let ns = whole
        .checked_mul(scale)
        .and_then(|v| v.checked_add(fractional_ns / denominator))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(|| validation(path, "duration exceeds unsigned 64-bit nanoseconds"))?;
    if ns == 0 && !zero {
        return invalid(path, "duration must be positive");
    }
    Ok(ns)
}

/// Parse an explicit positive duration without environment or filesystem access.
pub fn duration_ns(value: &str) -> Result<u64> {
    duration(&Value::String(value.to_owned()), "duration", false)
}
